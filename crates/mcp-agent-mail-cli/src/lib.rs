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

use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use chrono::{DateTime, Utc};

use mcp_agent_mail_core::{Config, resolve_project_identity};
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
}

pub type CliResult<T> = Result<T, CliError>;

#[derive(Parser, Debug)]
#[command(name = "am", version, about = "MCP Agent Mail CLI (Rust)")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
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
    },
    #[command(name = "serve-stdio")]
    ServeStdio,
    Lint,
    Typecheck,
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
    Migrate,
    #[command(name = "list-projects")]
    ListProjects {
        #[arg(long, default_value_t = false)]
        include_agents: bool,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(name = "clear-and-reset-everything")]
    ClearAndResetEverything {
        #[arg(long, short = 'f')]
        force: bool,
        #[arg(long, default_value_t = true)]
        archive: bool,
        #[arg(long = "no-archive", default_value_t = false)]
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
}

#[derive(Subcommand, Debug)]
pub enum ShareCommand {
    Export(ShareExportArgs),
    Update(ShareUpdateArgs),
    Preview(SharePreviewArgs),
    Verify(ShareVerifyArgs),
    Decrypt(ShareDecryptArgs),
    Wizard,
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

#[derive(Subcommand, Debug)]
pub enum ArchiveCommand {
    Save {
        #[arg(long = "project", short = 'p')]
        projects: Vec<String>,
        #[arg(long)]
        scrub_preset: Option<String>,
        #[arg(long, short = 'l')]
        label: Option<String>,
    },
    List {
        #[arg(long, short = 'n')]
        limit: Option<i64>,
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
    List {
        project: String,
        #[arg(long)]
        active_only: bool,
        #[arg(long = "all", default_value_t = false)]
        all: bool,
    },
    Active {
        project: String,
        #[arg(long)]
        limit: Option<i64>,
    },
    Soon {
        project: String,
        #[arg(long)]
        minutes: Option<i64>,
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
    #[arg(long)]
    pub shared: bool,
    #[arg(long, default_value_t = false)]
    pub block_on_conflicts: bool,
}

#[derive(Subcommand, Debug)]
pub enum ProjectsCommand {
    #[command(name = "mark-identity")]
    MarkIdentity {
        project_path: PathBuf,
        #[arg(long, default_value_t = true)]
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
    Status { project_path: PathBuf },
}

#[derive(Subcommand, Debug)]
pub enum ProductsCommand {
    Ensure {
        product_key: Option<String>,
        #[arg(long, short = 'n')]
        name: Option<String>,
    },
    Link {
        product_key: String,
        project: String,
    },
    Status {
        product_key: String,
    },
    Search {
        product_key: String,
        query: String,
        #[arg(long, short = 'l', default_value_t = 20)]
        limit: i64,
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
    },
    #[command(name = "summarize-thread")]
    SummarizeThread {
        product_key: String,
        thread_id: String,
        #[arg(long, short = 'n')]
        per_thread_limit: Option<i64>,
        #[arg(long)]
        no_llm: bool,
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
}

pub fn run() -> i32 {
    let cli = Cli::parse();
    match execute(cli) {
        Ok(()) => 0,
        Err(err) => {
            emit_error(&err);
            err_exit_code(&err)
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
    match cli.command {
        Commands::Share { action } => handle_share(action),
        Commands::Doctor { action } => handle_doctor(action),
        Commands::Guard { action } => handle_guard(action),
        Commands::FileReservations { action } => handle_file_reservations(action),
        Commands::Acks { action } => handle_acks(action),
        Commands::ListAcks { .. } => Err(CliError::NotImplemented("list-acks")),
        Commands::Archive { .. } => Err(CliError::NotImplemented("archive")),
        Commands::ServeHttp { host, port, path } => handle_serve_http(host, port, path),
        Commands::ServeStdio => handle_serve_stdio(),
        Commands::Lint => Err(CliError::NotImplemented("lint")),
        Commands::Typecheck => Err(CliError::NotImplemented("typecheck")),
        Commands::Migrate => Err(CliError::NotImplemented("migrate")),
        Commands::ListProjects { .. } => Err(CliError::NotImplemented("list-projects")),
        Commands::ClearAndResetEverything { .. } => {
            Err(CliError::NotImplemented("clear-and-reset-everything"))
        }
        Commands::Config { .. } => Err(CliError::NotImplemented("config")),
        Commands::Amctl { action } => handle_amctl(action),
        Commands::AmRun(args) => handle_am_run(args),
        Commands::Projects { .. } => Err(CliError::NotImplemented("projects")),
        Commands::Mail { .. } => Err(CliError::NotImplemented("mail")),
        Commands::Products { .. } => Err(CliError::NotImplemented("products")),
        Commands::Docs { .. } => Err(CliError::NotImplemented("docs")),
    }
}

fn handle_share(action: ShareCommand) -> CliResult<()> {
    match action {
        ShareCommand::Export(args) => {
            let _preset = share::normalize_scrub_preset(&args.scrub_preset)?;
            share::validate_thresholds(
                args.inline_threshold,
                args.detach_threshold,
                args.chunk_threshold,
                args.chunk_size,
            )?;
            if args.interactive {
                return Err(CliError::NotImplemented("share export --interactive"));
            }
            let inline = args.inline_threshold.max(0) as usize;
            let detach_raw = args.detach_threshold.max(0) as usize;
            let detach_adjusted = share::adjust_detach_threshold(inline, detach_raw);
            if detach_adjusted != detach_raw {
                ftui_runtime::ftui_eprintln!(
                    "warning: adjusted detach threshold to {} to exceed inline threshold",
                    detach_adjusted
                );
            }
            let _dry_run = resolve_bool(args.dry_run, args.no_dry_run, false);
            let _zip = resolve_bool(args.zip, args.no_zip, true);
            Err(CliError::NotImplemented("share export"))
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
            let _zip = resolve_bool(args.zip, args.no_zip, false);
            Err(CliError::NotImplemented("share update"))
        }
        ShareCommand::Preview(args) => {
            ensure_dir(&args.bundle)?;
            let _open = resolve_bool(args.open_browser, args.no_open_browser, false);
            Err(CliError::NotImplemented("share preview"))
        }
        ShareCommand::Verify(args) => {
            ensure_dir(&args.bundle)?;
            Err(CliError::NotImplemented("share verify"))
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
            let _output = args
                .output
                .unwrap_or_else(|| share::default_decrypt_output(&args.encrypted_path));
            Err(CliError::NotImplemented("share decrypt"))
        }
        ShareCommand::Wizard => Err(CliError::NotImplemented("share wizard")),
    }
}

fn handle_serve_http(
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
) -> CliResult<()> {
    let config = build_http_config(host, port, path);
    mcp_agent_mail_server::run_http(&config)?;
    Ok(())
}

fn handle_serve_stdio() -> CliResult<()> {
    let config = Config::from_env();
    mcp_agent_mail_server::run_stdio(&config);
    Ok(())
}

fn build_http_config(host: Option<String>, port: Option<u16>, path: Option<String>) -> Config {
    let mut config = Config::from_env();
    if let Some(host) = host {
        config.http_host = host;
    }
    if let Some(port) = port {
        config.http_port = port;
    }
    if let Some(path) = path {
        config.http_path = path;
    }
    config
}

fn handle_doctor(action: DoctorCommand) -> CliResult<()> {
    match action {
        DoctorCommand::Check { .. } => Err(CliError::NotImplemented("doctor check")),
        DoctorCommand::Repair { .. } => Err(CliError::NotImplemented("doctor repair")),
        DoctorCommand::Backups { .. } => Err(CliError::NotImplemented("doctor backups")),
        DoctorCommand::Restore { .. } => Err(CliError::NotImplemented("doctor restore")),
    }
}

fn handle_guard(action: GuardCommand) -> CliResult<()> {
    match action {
        GuardCommand::Install { project, repo, .. } => {
            mcp_agent_mail_guard::install_guard(&project, repo.as_path())?;
            Ok(())
        }
        GuardCommand::Uninstall { repo } => {
            mcp_agent_mail_guard::uninstall_guard(repo.as_path())?;
            Ok(())
        }
        GuardCommand::Status { .. } => Err(CliError::NotImplemented("guard status")),
        GuardCommand::Check { .. } => Err(CliError::NotImplemented("guard check")),
    }
}

fn handle_file_reservations(action: FileReservationsCommand) -> CliResult<()> {
    match action {
        FileReservationsCommand::List { .. } => {
            Err(CliError::NotImplemented("file_reservations list"))
        }
        FileReservationsCommand::Active { .. } => {
            Err(CliError::NotImplemented("file_reservations active"))
        }
        FileReservationsCommand::Soon { .. } => {
            Err(CliError::NotImplemented("file_reservations soon"))
        }
    }
}

fn handle_acks(action: AcksCommand) -> CliResult<()> {
    match action {
        AcksCommand::Pending { .. } => Err(CliError::NotImplemented("acks pending")),
        AcksCommand::Remind { .. } => Err(CliError::NotImplemented("acks remind")),
        AcksCommand::Overdue { .. } => Err(CliError::NotImplemented("acks overdue")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_http_overrides_are_applied() {
        let config = build_http_config(
            Some("0.0.0.0".to_string()),
            Some(9000),
            Some("/api/v2/".to_string()),
        );
        assert_eq!(config.http_host, "0.0.0.0");
        assert_eq!(config.http_port, 9000);
        assert_eq!(config.http_path, "/api/v2/");
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
        match cli.command {
            Commands::ServeHttp { host, port, path } => {
                assert_eq!(host.as_deref(), Some("0.0.0.0"));
                assert_eq!(port, Some(9999));
                assert_eq!(path.as_deref(), Some("/api/x/"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
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
    let identity = resolve_project_identity(args.path.to_string_lossy().as_ref());
    let agent_name = args
        .agent
        .or_else(|| std::env::var("AGENT_NAME").ok())
        .unwrap_or_else(|| "Unknown".to_string());
    let branch = identity
        .branch
        .clone()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    let cache_key = format!(
        "am-cache-{}-{}-{}",
        identity.project_uid, agent_name, branch
    );

    let slot_dir = ensure_slot_dir(&config, &identity.slug, &args.slot)?;
    let lease_path = lease_path(&slot_dir, &agent_name, &branch);

    let now = Utc::now();
    let expires = now + chrono::Duration::seconds(args.ttl_seconds.max(60));
    let lease = LeaseRecord {
        slot: args.slot.clone(),
        agent: agent_name.clone(),
        branch: branch.clone(),
        exclusive: !args.shared,
        acquired_ts: now.to_rfc3339(),
        expires_ts: expires.to_rfc3339(),
        released_ts: None,
    };
    let _ = write_lease(&lease_path, &lease);

    let renew_stop = Arc::new(AtomicBool::new(false));
    let mut renew_thread: Option<std::thread::JoinHandle<()>> = None;

    if config.worktrees_enabled {
        let conflicts = read_active_leases(&slot_dir, &agent_name, &branch, args.shared);
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
            if !args.shared && args.block_on_conflicts {
                return Err(CliError::ExitCode(1));
            }
        }

        let lease_path_clone = lease_path.clone();
        let slot_key = args.slot.clone();
        let agent_clone = agent_name.clone();
        let branch_clone = branch.clone();
        let shared = args.shared;
        let ttl = args.ttl_seconds.max(60);
        let stop_flag = Arc::clone(&renew_stop);
        renew_thread = Some(std::thread::spawn(move || {
            let interval = std::cmp::max(60, ttl / 2);
            while !stop_flag.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_secs(interval as u64));
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
                let now = Utc::now();
                let expires = now + chrono::Duration::seconds(interval);
                let mut updated = read_lease(&lease_path_clone).unwrap_or_else(|| LeaseRecord {
                    slot: slot_key.clone(),
                    agent: agent_clone.clone(),
                    branch: branch_clone.clone(),
                    exclusive: !shared,
                    acquired_ts: now.to_rfc3339(),
                    expires_ts: expires.to_rfc3339(),
                    released_ts: None,
                });
                updated.expires_ts = expires.to_rfc3339();
                let _ = write_lease(&lease_path_clone, &updated);
            }
        }));
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

    ftui_runtime::ftui_println!("$ {}  (slot={})", args.cmd.join(" "), args.slot);

    let status = cmd.status();
    let exit_code = match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 127,
        Err(_) => 1,
    };

    if config.worktrees_enabled {
        let now = Utc::now().to_rfc3339();
        if let Some(mut lease) = read_lease(&lease_path) {
            lease.released_ts = Some(now.clone());
            lease.expires_ts = now.clone();
            let _ = write_lease(&lease_path, &lease);
        }
        renew_stop.store(true, Ordering::SeqCst);
        if let Some(handle) = renew_thread {
            let _ = handle.join();
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
    if out.is_empty() {
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
        if let Some(exp) = parse_rfc3339(&lease.expires_ts) {
            if exp <= now {
                continue;
            }
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
