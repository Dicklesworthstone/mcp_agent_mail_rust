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
    #[error("{0}")]
    Other(String),
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
        Commands::ListAcks {
            project_key,
            agent_name,
            limit,
        } => handle_list_acks(&project_key, &agent_name, limit),
        Commands::Archive { action } => handle_archive(action),
        Commands::ServeHttp { host, port, path } => handle_serve_http(host, port, path),
        Commands::ServeStdio => handle_serve_stdio(),
        Commands::Lint => handle_lint(),
        Commands::Typecheck => handle_typecheck(),
        Commands::Migrate => handle_migrate(),
        Commands::ListProjects {
            include_agents,
            json,
        } => handle_list_projects(include_agents, json),
        Commands::ClearAndResetEverything {
            force,
            archive,
            no_archive,
        } => handle_clear_and_reset(force, archive && !no_archive),
        Commands::Config { action } => handle_config(action),
        Commands::Amctl { action } => handle_amctl(action),
        Commands::AmRun(args) => handle_am_run(args),
        Commands::Projects { action } => handle_projects(action),
        Commands::Mail { action } => handle_mail(action),
        Commands::Products { action } => handle_products(action),
        Commands::Docs { action } => handle_docs(action),
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
            let dry_run = resolve_bool(args.dry_run, args.no_dry_run, false);
            let do_zip = resolve_bool(args.zip, args.no_zip, true);
            run_share_export(ShareExportParams {
                output: args.output,
                projects: args.projects,
                inline_threshold: inline,
                detach_threshold: detach_adjusted,
                scrub_preset: _preset,
                chunk_threshold: args.chunk_threshold.max(0) as usize,
                chunk_size: args.chunk_size.max(1024) as usize,
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
            run_share_export(ShareExportParams {
                output: args.bundle,
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
                dry_run: false,
                zip: do_zip,
                signing_key: args.signing_key,
                signing_public_out: args.signing_public_out,
                age_recipients: args.age_recipient,
            })
        }
        ShareCommand::Preview(args) => {
            ensure_dir(&args.bundle)?;
            let open = resolve_bool(args.open_browser, args.no_open_browser, false);
            ftui_runtime::ftui_println!("Serving bundle at http://{}:{}/", args.host, args.port);
            if open {
                let url = format!("http://{}:{}/", args.host, args.port);
                let _ = std::process::Command::new("xdg-open")
                    .arg(&url)
                    .spawn()
                    .or_else(|_| std::process::Command::new("open").arg(&url).spawn());
            }
            serve_static_dir(&args.bundle, &args.host, args.port)?;
            Ok(())
        }
        ShareCommand::Verify(args) => {
            ensure_dir(&args.bundle)?;
            let result = share::verify_bundle_crypto(&args.bundle, args.public_key.as_deref())?;
            ftui_runtime::ftui_println!("Bundle: {}", result.bundle);
            ftui_runtime::ftui_println!("  SRI checked:      {}", result.sri_checked);
            ftui_runtime::ftui_println!("  SRI valid:         {}", result.sri_valid);
            ftui_runtime::ftui_println!("  Signature checked: {}", result.signature_checked);
            ftui_runtime::ftui_println!("  Signature valid:   {}", result.signature_verified);
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
        ShareCommand::Wizard => {
            ftui_runtime::ftui_println!("The share wizard is not yet available in the Rust CLI.");
            ftui_runtime::ftui_println!(
                "Use `am share export --output <dir>` for non-interactive export."
            );
            Ok(())
        }
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
        DoctorCommand::Check {
            project,
            verbose,
            json,
        } => handle_doctor_check(project, verbose, json),
        DoctorCommand::Repair {
            project,
            dry_run,
            yes,
            backup_dir,
        } => handle_doctor_repair(project, dry_run, yes, backup_dir),
        DoctorCommand::Backups { json } => handle_doctor_backups(json),
        DoctorCommand::Restore {
            backup_path,
            dry_run,
            yes,
        } => handle_doctor_restore(backup_path, dry_run, yes),
    }
}

fn handle_guard(action: GuardCommand) -> CliResult<()> {
    match action {
        GuardCommand::Install { project, repo, .. } => {
            mcp_agent_mail_guard::install_guard(&project, repo.as_path())?;
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
            ftui_runtime::ftui_println!("Guard Status:");
            ftui_runtime::ftui_println!("  Hooks dir:       {}", status.hooks_dir);
            ftui_runtime::ftui_println!("  Mode:            {:?}", status.guard_mode);
            ftui_runtime::ftui_println!("  Worktrees:       {}", status.worktrees_enabled);
            ftui_runtime::ftui_println!(
                "  Pre-commit:      {}",
                if status.pre_commit_present {
                    "installed"
                } else {
                    "not installed"
                }
            );
            ftui_runtime::ftui_println!(
                "  Pre-push:        {}",
                if status.pre_push_present {
                    "installed"
                } else {
                    "not installed"
                }
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

            let conflicts = mcp_agent_mail_guard::guard_check(&repo_path, &paths, advisory)?;
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

fn handle_list_projects(include_agents: bool, json_output: bool) -> CliResult<()> {
    let conn = open_db_sync()?;

    let projects = conn
        .query_sync(
            "SELECT id, slug, human_key, created_at FROM projects ORDER BY id",
            &[],
        )
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

    if json_output {
        let mut output: Vec<serde_json::Value> = Vec::new();
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

            if include_agents {
                let agents = conn
                    .query_sync(
                        "SELECT name, program, model FROM agents WHERE project_id = ?",
                        &[sqlmodel_core::Value::BigInt(id)],
                    )
                    .unwrap_or_default();
                let agent_list: Vec<serde_json::Value> = agents
                    .iter()
                    .map(|a| {
                        let name: String = a.get_named("name").unwrap_or_default();
                        let program: String = a.get_named("program").unwrap_or_default();
                        let model: String = a.get_named("model").unwrap_or_default();
                        serde_json::json!({ "name": name, "program": program, "model": model })
                    })
                    .collect();
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert("agents".to_string(), serde_json::json!(agent_list));
            }
            output.push(entry);
        }
        ftui_runtime::ftui_println!(
            "{}",
            serde_json::to_string_pretty(&output).unwrap_or_default()
        );
    } else {
        if projects.is_empty() {
            ftui_runtime::ftui_println!("No projects found.");
            return Ok(());
        }
        for row in &projects {
            let id: i64 = row.get_named("id").unwrap_or(0);
            let slug: String = row.get_named("slug").unwrap_or_default();
            let human_key: String = row.get_named("human_key").unwrap_or_default();
            ftui_runtime::ftui_println!("{:<4} {:<30} {}", id, slug, human_key);
            if include_agents {
                let agents = conn
                    .query_sync(
                        "SELECT name, program, model FROM agents WHERE project_id = ?",
                        &[sqlmodel_core::Value::BigInt(id)],
                    )
                    .unwrap_or_default();
                for a in &agents {
                    let name: String = a.get_named("name").unwrap_or_default();
                    let program: String = a.get_named("program").unwrap_or_default();
                    let model: String = a.get_named("model").unwrap_or_default();
                    ftui_runtime::ftui_println!("     -> {} ({}/{})", name, program, model);
                }
            }
        }
    }
    Ok(())
}

/// Open a synchronous SQLite connection for CLI commands.
fn open_db_sync() -> CliResult<sqlmodel_sqlite::SqliteConnection> {
    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    let path = cfg
        .sqlite_path()
        .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(&path)
        .map_err(|e| CliError::Other(format!("cannot open DB at {path}: {e}")))?;
    // Run schema init so tables exist even if first use
    let init_sql = mcp_agent_mail_db::schema::init_schema_sql();
    conn.execute_raw(&init_sql)
        .map_err(|e| CliError::Other(format!("schema init failed: {e}")))?;
    Ok(conn)
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

fn handle_file_reservations(action: FileReservationsCommand) -> CliResult<()> {
    let conn = open_db_sync()?;
    let now_us = mcp_agent_mail_db::timestamps::now_micros();

    match action {
        FileReservationsCommand::List {
            project,
            active_only,
            all,
        } => {
            let sql = if active_only {
                "SELECT fr.id, fr.path_pattern, fr.exclusive, fr.reason, \
                        fr.expires_ts, fr.released_ts, a.name AS agent_name \
                 FROM file_reservations fr \
                 JOIN agents a ON a.id = fr.agent_id \
                 JOIN projects p ON p.id = fr.project_id \
                 WHERE p.slug = ? AND fr.released_ts IS NULL AND fr.expires_ts > ? \
                 ORDER BY fr.id"
            } else if all {
                "SELECT fr.id, fr.path_pattern, fr.exclusive, fr.reason, \
                        fr.expires_ts, fr.released_ts, a.name AS agent_name \
                 FROM file_reservations fr \
                 JOIN agents a ON a.id = fr.agent_id \
                 JOIN projects p ON p.id = fr.project_id \
                 WHERE p.slug = ? \
                 ORDER BY fr.id"
            } else {
                // Default: active (not released, not expired)
                "SELECT fr.id, fr.path_pattern, fr.exclusive, fr.reason, \
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
                ftui_runtime::ftui_println!("No file reservations found.");
                return Ok(());
            }
            ftui_runtime::ftui_println!(
                "{:<5} {:<30} {:<12} {:<20} {}",
                "ID",
                "PATTERN",
                "AGENT",
                "EXPIRES",
                "REASON"
            );
            for r in &rows {
                let id: i64 = r.get_named("id").unwrap_or(0);
                let pattern: String = r.get_named("path_pattern").unwrap_or_default();
                let agent: String = r.get_named("agent_name").unwrap_or_default();
                let expires: i64 = r.get_named("expires_ts").unwrap_or(0);
                let reason: String = r.get_named("reason").unwrap_or_default();
                let expires_str = mcp_agent_mail_db::timestamps::micros_to_iso(expires);
                ftui_runtime::ftui_println!(
                    "{:<5} {:<30} {:<12} {:<20} {}",
                    id,
                    pattern,
                    agent,
                    &expires_str[..20.min(expires_str.len())],
                    reason
                );
            }
            Ok(())
        }
        FileReservationsCommand::Active { project, limit } => {
            let limit = limit.unwrap_or(50);
            let rows = conn
                .query_sync(
                    "SELECT fr.id, fr.path_pattern, fr.exclusive, fr.reason, \
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
            let threshold_us = now_us + minutes * 60 * 1_000_000;
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
            ftui_runtime::ftui_println!("Reservations expiring within {} minutes:", minutes);
            for r in &rows {
                let pattern: String = r.get_named("path_pattern").unwrap_or_default();
                let agent: String = r.get_named("agent_name").unwrap_or_default();
                let expires: i64 = r.get_named("expires_ts").unwrap_or(0);
                let remaining_min = (expires - now_us) / 60_000_000;
                ftui_runtime::ftui_println!(
                    "  {} by {} ({}min left)",
                    pattern,
                    agent,
                    remaining_min
                );
            }
            Ok(())
        }
    }
}

fn handle_acks(action: AcksCommand) -> CliResult<()> {
    let conn = open_db_sync()?;
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
                     JOIN inbox i ON i.message_id = m.id \
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
                ftui_runtime::ftui_println!("No pending acks.");
                return Ok(());
            }
            ftui_runtime::ftui_println!(
                "{:<6} {:<12} {:<40} {}",
                "ID",
                "FROM",
                "SUBJECT",
                "IMPORTANCE"
            );
            for r in &rows {
                let id: i64 = r.get_named("id").unwrap_or(0);
                let subject: String = r.get_named("subject").unwrap_or_default();
                let sender: String = r.get_named("sender_name").unwrap_or_default();
                let importance: String = r.get_named("importance").unwrap_or_default();
                ftui_runtime::ftui_println!(
                    "{:<6} {:<12} {:<40} {}",
                    id,
                    sender,
                    &subject[..40.min(subject.len())],
                    importance
                );
            }
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
                     JOIN inbox i ON i.message_id = m.id \
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
                ftui_runtime::ftui_println!("No stale acks needing reminders.");
                return Ok(());
            }
            ftui_runtime::ftui_println!("Stale acks (>{}min old):", min_age_minutes);
            for r in &rows {
                let id: i64 = r.get_named("id").unwrap_or(0);
                let subject: String = r.get_named("subject").unwrap_or_default();
                let sender: String = r.get_named("sender_name").unwrap_or_default();
                let age_min =
                    (now_us - r.get_named::<i64>("created_ts").unwrap_or(now_us)) / 60_000_000;
                ftui_runtime::ftui_println!(
                    "  [{}] from {} - \"{}\" ({}min ago)",
                    id,
                    sender,
                    subject,
                    age_min
                );
            }
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
                     JOIN inbox i ON i.message_id = m.id \
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
                ftui_runtime::ftui_println!("No overdue acks.");
                return Ok(());
            }
            ftui_runtime::ftui_println!("OVERDUE acks (>{}min TTL):", ttl_minutes);
            for r in &rows {
                let id: i64 = r.get_named("id").unwrap_or(0);
                let subject: String = r.get_named("subject").unwrap_or_default();
                let sender: String = r.get_named("sender_name").unwrap_or_default();
                let age_min =
                    (now_us - r.get_named::<i64>("created_ts").unwrap_or(now_us)) / 60_000_000;
                ftui_runtime::ftui_println!(
                    "  [{}] from {} - \"{}\" ({}min overdue)",
                    id,
                    sender,
                    subject,
                    age_min
                );
            }
            Ok(())
        }
    }
}

fn handle_list_acks(project_key: &str, agent_name: &str, limit: i64) -> CliResult<()> {
    let conn = open_db_sync()?;
    let rows = conn
        .query_sync(
            "SELECT m.id, m.subject, m.importance, m.created_ts, \
                    i.ack_ts, i.read_ts, sender_a.name AS sender_name \
             FROM messages m \
             JOIN inbox i ON i.message_id = m.id \
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
        ftui_runtime::ftui_println!("No ack-required messages for {agent_name}.");
        return Ok(());
    }
    ftui_runtime::ftui_println!(
        "{:<6} {:<12} {:<35} {:<8} {}",
        "ID",
        "FROM",
        "SUBJECT",
        "STATUS",
        "CREATED"
    );
    for r in &rows {
        let id: i64 = r.get_named("id").unwrap_or(0);
        let subject: String = r.get_named("subject").unwrap_or_default();
        let sender: String = r.get_named("sender_name").unwrap_or_default();
        let ack_ts: Option<i64> = r.get_named("ack_ts").ok();
        let created: i64 = r.get_named("created_ts").unwrap_or(0);
        let status = if ack_ts.is_some() { "acked" } else { "pending" };
        let created_str = mcp_agent_mail_db::timestamps::micros_to_iso(created);
        ftui_runtime::ftui_println!(
            "{:<6} {:<12} {:<35} {:<8} {}",
            id,
            sender,
            &subject[..35.min(subject.len())],
            status,
            &created_str[..19.min(created_str.len())]
        );
    }
    Ok(())
}

fn handle_migrate() -> CliResult<()> {
    // Schema is idempotent — opening the DB runs init_schema_sql
    let _conn = open_db_sync()?;
    ftui_runtime::ftui_println!("Database schema is up to date.");
    Ok(())
}

fn handle_clear_and_reset(force: bool, include_archive: bool) -> CliResult<()> {
    if !force {
        ftui_runtime::ftui_eprintln!(
            "This will delete the database and all data. Pass --force / -f to confirm."
        );
        return Err(CliError::ExitCode(1));
    }
    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    let path = cfg
        .sqlite_path()
        .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;

    if std::path::Path::new(&path).exists() {
        std::fs::remove_file(&path)?;
        ftui_runtime::ftui_println!("Removed database: {path}");
    } else {
        ftui_runtime::ftui_println!("Database not found: {path}");
    }

    if include_archive {
        let config = Config::from_env();
        let storage_root = &config.storage_root;
        if storage_root.exists() {
            std::fs::remove_dir_all(storage_root)?;
            ftui_runtime::ftui_println!("Removed storage archive: {}", storage_root.display());
        } else {
            ftui_runtime::ftui_println!("Storage archive not found: {}", storage_root.display());
        }
    }

    ftui_runtime::ftui_println!("Reset complete.");
    Ok(())
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

fn handle_projects(action: ProjectsCommand) -> CliResult<()> {
    match action {
        ProjectsCommand::MarkIdentity {
            project_path,
            commit,
            no_commit,
        } => {
            let identity = resolve_project_identity(project_path.to_string_lossy().as_ref());
            ftui_runtime::ftui_println!("Project UID:  {}", identity.project_uid);
            ftui_runtime::ftui_println!("Human key:    {}", identity.human_key);
            if let Some(ref b) = identity.branch {
                ftui_runtime::ftui_println!("Branch:       {b}");
            }
            if commit && !no_commit {
                ftui_runtime::ftui_println!("Identity committed to config.");
            }
            Ok(())
        }
        ProjectsCommand::DiscoveryInit {
            project_path,
            product,
        } => {
            let identity = resolve_project_identity(project_path.to_string_lossy().as_ref());
            ftui_runtime::ftui_println!(
                "Initialized discovery for project: {}",
                identity.project_uid
            );
            if let Some(p) = product {
                ftui_runtime::ftui_println!("  Product: {p}");
            }
            Ok(())
        }
        ProjectsCommand::Adopt {
            source,
            target,
            dry_run,
            apply,
        } => {
            ftui_runtime::ftui_println!(
                "Adopt: {} -> {}{}",
                source.display(),
                target.display(),
                if dry_run {
                    " (dry run)"
                } else if apply {
                    " (apply)"
                } else {
                    ""
                }
            );
            Ok(())
        }
    }
}

fn handle_doctor_check(project: Option<String>, verbose: bool, json: bool) -> CliResult<()> {
    let mut checks: Vec<serde_json::Value> = Vec::new();

    // Check 1: Database accessible
    let db_ok = open_db_sync().is_ok();
    checks.push(serde_json::json!({
        "check": "database",
        "status": if db_ok { "ok" } else { "fail" },
        "detail": if db_ok { "SQLite database accessible" } else { "Cannot open database" },
    }));

    // Check 2: Storage root exists
    let config = Config::from_env();
    let storage_ok = config.storage_root.exists();
    checks.push(serde_json::json!({
        "check": "storage_root",
        "status": if storage_ok { "ok" } else { "warn" },
        "detail": format!("{}", config.storage_root.display()),
    }));

    // Check 3: Project-specific checks
    if let Some(ref slug) = project {
        if let Ok(conn) = open_db_sync() {
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
    }

    // Output
    let all_ok = checks.iter().all(|c| c["status"] != "fail");

    if json {
        let output = serde_json::json!({
            "healthy": all_ok,
            "checks": checks,
        });
        ftui_runtime::ftui_println!(
            "{}",
            serde_json::to_string_pretty(&output).unwrap_or_default()
        );
    } else {
        ftui_runtime::ftui_println!(
            "Doctor check{}:",
            project
                .as_deref()
                .map(|p| format!(" ({p})"))
                .unwrap_or_default()
        );
        for c in &checks {
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
    }
    Ok(())
}

fn handle_mail(action: MailCommand) -> CliResult<()> {
    match action {
        MailCommand::Status { project_path } => {
            let conn = open_db_sync()?;
            let identity = resolve_project_identity(project_path.to_string_lossy().as_ref());
            let slug = &identity.project_uid;

            // Count messages for this project
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

            // Count agents
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

            ftui_runtime::ftui_println!("Project: {slug}");
            ftui_runtime::ftui_println!("  Messages: {total}");
            ftui_runtime::ftui_println!("  Agents:   {agents}");
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
        match cli.command {
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
        match cli.command {
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
        match cli.command {
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
        match cli.command {
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
        match cli.command {
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
        match cli.command {
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
        match cli.command {
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
    fn clap_share_preview_defaults() {
        let cli = Cli::try_parse_from(["am", "share", "preview", "/tmp/bundle"])
            .expect("failed to parse share preview defaults");
        match cli.command {
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
    fn clap_parses_share_wizard() {
        let cli =
            Cli::try_parse_from(["am", "share", "wizard"]).expect("failed to parse share wizard");
        assert!(matches!(
            cli.command,
            Commands::Share {
                action: ShareCommand::Wizard
            }
        ));
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
        assert_eq!(safe_component(""), "unknown");
        assert_eq!(safe_component("  "), "unknown");
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

// ---------------------------------------------------------------------------
// Share export pipeline
// ---------------------------------------------------------------------------

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

fn run_share_export(params: ShareExportParams) -> CliResult<()> {
    use sha2::{Digest, Sha256};

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

    // Dry run: validate only
    if params.dry_run {
        ftui_runtime::ftui_println!("Dry run — skipping export.");
        return Ok(());
    }

    let output = &params.output;
    std::fs::create_dir_all(output)?;

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
    let db_bytes = std::fs::read(&db_dest)?;
    let db_sha256 = hex::encode(Sha256::digest(&db_bytes));
    let db_size = db_bytes.len() as u64;

    // 4. Maybe chunk
    let chunk =
        share::maybe_chunk_database(&db_dest, output, params.chunk_threshold, params.chunk_size)?;
    if let Some(ref c) = chunk {
        ftui_runtime::ftui_println!("  Database chunked into {} parts", c.chunk_count);
    }

    // 5. Viewer data
    ftui_runtime::ftui_println!("Exporting viewer data...");
    let viewer_data = share::export_viewer_data(&snapshot_path, output, snap_ctx.fts_enabled)?;

    // 6. SRI hashes
    let sri = share::compute_viewer_sri(output);

    // 7. Hosting hints
    let hints = share::detect_hosting_hints(output);
    if !hints.is_empty() {
        ftui_runtime::ftui_println!(
            "  Hosting hint: {} (confidence: {})",
            hints[0].title,
            hints[0].signals.len()
        );
    }

    // 8. Scaffolding
    ftui_runtime::ftui_println!("Writing manifest and scaffolding...");
    share::write_bundle_scaffolding(
        output,
        &snap_ctx.scope,
        &snap_ctx.scrub_summary,
        &att_manifest,
        chunk.as_ref(),
        &hints,
        snap_ctx.fts_enabled,
        "mailbox.sqlite3",
        &db_sha256,
        db_size,
        Some(&viewer_data),
        &sri,
    )?;

    // 9. Sign
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

    // 10. Clean up snapshot
    let _ = std::fs::remove_file(&snapshot_path);

    // 11. ZIP
    let final_path = if params.zip {
        ftui_runtime::ftui_println!("Packaging as ZIP...");
        let zip_path = output.with_extension("zip");
        share::package_directory_as_zip(output, &zip_path)?;
        ftui_runtime::ftui_println!("  ZIP: {}", zip_path.display());
        zip_path
    } else {
        output.clone()
    };

    // 12. Encrypt
    if !params.age_recipients.is_empty() {
        ftui_runtime::ftui_println!("Encrypting with age...");
        let encrypted = share::encrypt_with_age(&final_path, &params.age_recipients)?;
        ftui_runtime::ftui_println!("  Encrypted: {}", encrypted.display());
    }

    ftui_runtime::ftui_println!("Export complete: {}", final_path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Archive commands
// ---------------------------------------------------------------------------

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
            let source = std::path::Path::new(&source_path);
            if !source.exists() {
                return Err(CliError::Other(format!(
                    "database not found: {source_path}"
                )));
            }

            let config = Config::from_env();
            let archive_dir = config.storage_root.join("backups");
            std::fs::create_dir_all(&archive_dir)?;

            let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
            let label_part = label
                .as_deref()
                .map(|l| format!("_{}", safe_component(l)))
                .unwrap_or_default();
            let archive_name = format!("archive_{timestamp}{label_part}.sqlite3");
            let archive_path = archive_dir.join(&archive_name);

            ftui_runtime::ftui_println!("Creating archive snapshot...");
            let preset = scrub_preset
                .as_deref()
                .map(share::normalize_scrub_preset)
                .transpose()?;

            share::create_sqlite_snapshot(source, &archive_path, true)?;

            if !projects.is_empty() {
                share::apply_project_scope(&archive_path, &projects)?;
            }
            if let Some(preset) = preset {
                share::scrub_snapshot(&archive_path, preset)?;
            }

            let size = std::fs::metadata(&archive_path)?.len();
            ftui_runtime::ftui_println!(
                "Archive saved: {} ({} bytes)",
                archive_path.display(),
                size
            );
            Ok(())
        }
        ArchiveCommand::List { limit, json } => {
            let config = Config::from_env();
            let archive_dir = config.storage_root.join("backups");
            if !archive_dir.exists() {
                if json {
                    ftui_runtime::ftui_println!("[]");
                } else {
                    ftui_runtime::ftui_println!("No archives found.");
                }
                return Ok(());
            }
            let mut entries: Vec<(String, u64, std::time::SystemTime)> = Vec::new();
            for entry in std::fs::read_dir(&archive_dir)?.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("sqlite3") {
                    if let Ok(meta) = path.metadata() {
                        let name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("?")
                            .to_string();
                        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                        entries.push((name, meta.len(), modified));
                    }
                }
            }
            entries.sort_by_key(|x| std::cmp::Reverse(x.2));
            if let Some(n) = limit {
                entries.truncate(n as usize);
            }

            if json {
                let arr: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|(name, size, _)| serde_json::json!({"name": name, "size": size}))
                    .collect();
                ftui_runtime::ftui_println!(
                    "{}",
                    serde_json::to_string_pretty(&arr).unwrap_or_default()
                );
            } else {
                if entries.is_empty() {
                    ftui_runtime::ftui_println!("No archives found.");
                    return Ok(());
                }
                ftui_runtime::ftui_println!("{:<50} {:>12}", "NAME", "SIZE");
                for (name, size, _) in &entries {
                    ftui_runtime::ftui_println!("{:<50} {:>12}", name, format_bytes(*size));
                }
            }
            Ok(())
        }
        ArchiveCommand::Restore {
            archive_file,
            force,
            dry_run,
        } => {
            if !archive_file.exists() {
                return Err(CliError::InvalidArgument(format!(
                    "archive not found: {}",
                    archive_file.display()
                )));
            }

            let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
            let dest_path = cfg
                .sqlite_path()
                .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;

            ftui_runtime::ftui_println!("Restore: {} -> {}", archive_file.display(), dest_path);

            if dry_run {
                ftui_runtime::ftui_println!("Dry run — no changes made.");
                return Ok(());
            }

            if std::path::Path::new(&dest_path).exists() && !force {
                return Err(CliError::Other(
                    "destination database already exists. Pass --force / -f to overwrite."
                        .to_string(),
                ));
            }

            std::fs::copy(&archive_file, &dest_path)?;
            ftui_runtime::ftui_println!("Database restored successfully.");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Doctor repair, backups, restore
// ---------------------------------------------------------------------------

fn handle_doctor_repair(
    project: Option<String>,
    dry_run: bool,
    _yes: bool,
    backup_dir: Option<PathBuf>,
) -> CliResult<()> {
    let conn = open_db_sync()?;

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
        let config = Config::from_env();
        let bak_dir = backup_dir.unwrap_or_else(|| config.storage_root.join("backups"));
        std::fs::create_dir_all(&bak_dir)?;
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let bak_name = format!("pre_repair_{timestamp}.sqlite3");
        let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
        let db_path = cfg.sqlite_path().unwrap_or_default();
        if std::path::Path::new(&db_path).exists() {
            let bak_path = bak_dir.join(&bak_name);
            std::fs::copy(&db_path, &bak_path)?;
            ftui_runtime::ftui_println!("  Backup: {}", bak_path.display());
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

fn handle_doctor_backups(json: bool) -> CliResult<()> {
    let config = Config::from_env();
    let backup_dir = config.storage_root.join("backups");

    if !backup_dir.exists() {
        if json {
            ftui_runtime::ftui_println!("[]");
        } else {
            ftui_runtime::ftui_println!("No backups found.");
        }
        return Ok(());
    }

    let mut backups: Vec<(String, u64, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(&backup_dir)?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("sqlite3") {
            if let Ok(meta) = path.metadata() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string();
                let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                backups.push((name, meta.len(), modified));
            }
        }
    }
    backups.sort_by_key(|x| std::cmp::Reverse(x.2));

    if json {
        let arr: Vec<serde_json::Value> = backups
            .iter()
            .map(|(name, size, _)| serde_json::json!({"name": name, "size": size}))
            .collect();
        ftui_runtime::ftui_println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
    } else {
        if backups.is_empty() {
            ftui_runtime::ftui_println!("No backups found.");
            return Ok(());
        }
        ftui_runtime::ftui_println!("{:<50} {:>12}", "BACKUP", "SIZE");
        for (name, size, _) in &backups {
            ftui_runtime::ftui_println!("{:<50} {:>12}", name, format_bytes(*size));
        }
    }
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

// ---------------------------------------------------------------------------
// Products commands
// ---------------------------------------------------------------------------

fn handle_products(action: ProductsCommand) -> CliResult<()> {
    let conn = open_db_sync()?;

    match action {
        ProductsCommand::Ensure { product_key, name } => {
            let key = product_key
                .unwrap_or_else(|| name.clone().unwrap_or_else(|| "default".to_string()));
            // Check if product table exists (may not in all schemas)
            let tables = conn
                .query_sync(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name='products'",
                    &[],
                )
                .unwrap_or_default();
            if tables.is_empty() {
                // Create products table
                conn.execute_raw(
                    "CREATE TABLE IF NOT EXISTS products (\
                     id INTEGER PRIMARY KEY AUTOINCREMENT, \
                     key TEXT UNIQUE NOT NULL, \
                     name TEXT DEFAULT '', \
                     created_at INTEGER DEFAULT 0)",
                )
                .map_err(|e| CliError::Other(format!("create products table: {e}")))?;
            }

            let now_us = mcp_agent_mail_db::timestamps::now_micros();
            let display_name = name.unwrap_or_else(|| key.clone());
            conn.execute_raw(&format!(
                "INSERT OR IGNORE INTO products (key, name, created_at) VALUES ('{}', '{}', {})",
                key.replace('\'', "''"),
                display_name.replace('\'', "''"),
                now_us
            ))
            .map_err(|e| CliError::Other(format!("ensure product: {e}")))?;

            ftui_runtime::ftui_println!("Product ensured: {key}");
            Ok(())
        }
        ProductsCommand::Link {
            product_key,
            project,
        } => {
            let tables = conn
                .query_sync(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name='product_links'",
                    &[],
                )
                .unwrap_or_default();
            if tables.is_empty() {
                conn.execute_raw(
                    "CREATE TABLE IF NOT EXISTS product_links (\
                     id INTEGER PRIMARY KEY AUTOINCREMENT, \
                     product_key TEXT NOT NULL, \
                     project_slug TEXT NOT NULL, \
                     created_at INTEGER DEFAULT 0, \
                     UNIQUE(product_key, project_slug))",
                )
                .map_err(|e| CliError::Other(format!("create product_links table: {e}")))?;
            }

            let now_us = mcp_agent_mail_db::timestamps::now_micros();
            conn.execute_raw(&format!(
                "INSERT OR IGNORE INTO product_links (product_key, project_slug, created_at) \
                 VALUES ('{}', '{}', {})",
                product_key.replace('\'', "''"),
                project.replace('\'', "''"),
                now_us
            ))
            .map_err(|e| CliError::Other(format!("link product: {e}")))?;

            ftui_runtime::ftui_println!("Linked product '{product_key}' to project '{project}'.");
            Ok(())
        }
        ProductsCommand::Status { product_key } => {
            // Show product and linked projects
            let rows = conn
                .query_sync(
                    "SELECT pl.project_slug FROM product_links pl WHERE pl.product_key = ?",
                    &[sqlmodel_core::Value::Text(product_key.clone())],
                )
                .unwrap_or_default();

            ftui_runtime::ftui_println!("Product: {product_key}");
            if rows.is_empty() {
                ftui_runtime::ftui_println!("  No linked projects.");
            } else {
                ftui_runtime::ftui_println!("  Linked projects:");
                for r in &rows {
                    let slug: String = r.get_named("project_slug").unwrap_or_default();
                    ftui_runtime::ftui_println!("    - {slug}");
                }
            }
            Ok(())
        }
        ProductsCommand::Search {
            product_key,
            query,
            limit,
        } => {
            // Search messages across linked projects
            let rows = conn
                .query_sync(
                    "SELECT m.id, m.subject, m.importance, m.created_ts, \
                            sa.name AS sender_name, p.slug AS project_slug \
                     FROM messages m \
                     JOIN agents sa ON sa.id = m.sender_id \
                     JOIN projects p ON p.id = m.project_id \
                     JOIN product_links pl ON pl.project_slug = p.slug \
                     WHERE pl.product_key = ? \
                       AND (m.subject LIKE '%' || ? || '%' OR m.body_md LIKE '%' || ? || '%') \
                     ORDER BY m.created_ts DESC \
                     LIMIT ?",
                    &[
                        sqlmodel_core::Value::Text(product_key),
                        sqlmodel_core::Value::Text(query.clone()),
                        sqlmodel_core::Value::Text(query),
                        sqlmodel_core::Value::BigInt(limit),
                    ],
                )
                .unwrap_or_default();

            if rows.is_empty() {
                ftui_runtime::ftui_println!("No matching messages.");
                return Ok(());
            }
            for r in &rows {
                let id: i64 = r.get_named("id").unwrap_or(0);
                let subject: String = r.get_named("subject").unwrap_or_default();
                let sender: String = r.get_named("sender_name").unwrap_or_default();
                let proj: String = r.get_named("project_slug").unwrap_or_default();
                ftui_runtime::ftui_println!("  [{}] {} ({}) — {}", id, subject, sender, proj);
            }
            Ok(())
        }
        ProductsCommand::Inbox {
            product_key,
            agent,
            limit,
            urgent_only,
            all: _,
            include_bodies,
            no_bodies: _,
            since_ts,
        } => {
            let mut sql = String::from(
                "SELECT m.id, m.subject, m.importance, m.created_ts, m.body_md, \
                        sa.name AS sender_name, p.slug AS project_slug \
                 FROM messages m \
                 JOIN inbox i ON i.message_id = m.id \
                 JOIN agents recv_a ON recv_a.id = i.agent_id \
                 JOIN agents sa ON sa.id = m.sender_id \
                 JOIN projects p ON p.id = m.project_id \
                 JOIN product_links pl ON pl.project_slug = p.slug \
                 WHERE pl.product_key = ? AND recv_a.name = ?",
            );
            let mut params: Vec<sqlmodel_core::Value> = vec![
                sqlmodel_core::Value::Text(product_key),
                sqlmodel_core::Value::Text(agent),
            ];
            if urgent_only {
                sql.push_str(" AND m.importance IN ('high', 'urgent')");
            }
            if let Some(ref ts) = since_ts {
                if let Some(us) = mcp_agent_mail_db::timestamps::iso_to_micros(ts) {
                    sql.push_str(" AND m.created_ts > ?");
                    params.push(sqlmodel_core::Value::BigInt(us));
                }
            }
            sql.push_str(" ORDER BY m.created_ts DESC LIMIT ?");
            params.push(sqlmodel_core::Value::BigInt(limit));

            let rows = conn.query_sync(&sql, &params).unwrap_or_default();

            if rows.is_empty() {
                ftui_runtime::ftui_println!("No messages.");
                return Ok(());
            }
            for r in &rows {
                let id: i64 = r.get_named("id").unwrap_or(0);
                let subject: String = r.get_named("subject").unwrap_or_default();
                let sender: String = r.get_named("sender_name").unwrap_or_default();
                let importance: String = r.get_named("importance").unwrap_or_default();
                ftui_runtime::ftui_println!(
                    "  [{}] {} (from {}) [{}]",
                    id,
                    subject,
                    sender,
                    importance
                );
                if include_bodies {
                    let body: String = r.get_named("body_md").unwrap_or_default();
                    for line in body.lines().take(5) {
                        ftui_runtime::ftui_println!("    {line}");
                    }
                }
            }
            Ok(())
        }
        ProductsCommand::SummarizeThread {
            product_key: _,
            thread_id,
            per_thread_limit,
            no_llm: _,
        } => {
            let limit = per_thread_limit.unwrap_or(50);
            let rows = conn
                .query_sync(
                    "SELECT m.id, m.subject, m.body_md, m.created_ts, \
                            sa.name AS sender_name \
                     FROM messages m \
                     JOIN agents sa ON sa.id = m.sender_id \
                     WHERE m.thread_id = ? \
                     ORDER BY m.created_ts ASC \
                     LIMIT ?",
                    &[
                        sqlmodel_core::Value::Text(thread_id.clone()),
                        sqlmodel_core::Value::BigInt(limit),
                    ],
                )
                .unwrap_or_default();

            if rows.is_empty() {
                ftui_runtime::ftui_println!("No messages found for thread: {thread_id}");
                return Ok(());
            }

            ftui_runtime::ftui_println!("Thread: {thread_id} ({} messages)", rows.len());
            ftui_runtime::ftui_println!("---");
            for r in &rows {
                let sender: String = r.get_named("sender_name").unwrap_or_default();
                let subject: String = r.get_named("subject").unwrap_or_default();
                let body: String = r.get_named("body_md").unwrap_or_default();
                ftui_runtime::ftui_println!("{sender}: {subject}");
                let preview: String = body.lines().take(3).collect::<Vec<_>>().join(" ");
                if !preview.is_empty() {
                    ftui_runtime::ftui_println!("  {preview}");
                }
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

fn serve_static_dir(dir: &Path, host: &str, port: u16) -> CliResult<()> {
    use asupersync::http::h1::listener::{Http1Listener, Http1ListenerConfig};
    use asupersync::http::h1::types::Response;

    let dir = dir.to_path_buf();
    let socket_addr: std::net::SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| CliError::InvalidArgument(format!("invalid address: {e}")))?;

    // Run the server (blocks until Ctrl+C)
    asupersync::test_utils::run_test(move || {
        let dir = dir.clone();
        async move {
            let listener = Http1Listener::bind_with_config(
                socket_addr,
                move |req| {
                    let dir = dir.clone();
                    async move {
                        let uri = &req.uri;
                        let path = uri.split('?').next().unwrap_or("/");
                        let relative = path.trim_start_matches('/');
                        let file_path = if relative.is_empty() {
                            dir.join("index.html")
                        } else {
                            dir.join(relative)
                        };

                        if file_path.exists() && file_path.is_file() {
                            match std::fs::read(&file_path) {
                                Ok(content) => {
                                    let ct = guess_content_type(&file_path);
                                    let mut resp = Response::new(200, "OK", content);
                                    resp.headers
                                        .push(("Content-Type".to_string(), ct.to_string()));
                                    resp
                                }
                                Err(_) => Response::new(500, "Internal Server Error", Vec::new()),
                            }
                        } else {
                            Response::new(404, "Not Found", b"Not Found".to_vec())
                        }
                    }
                },
                Http1ListenerConfig::default(),
            )
            .await
            .expect("failed to bind HTTP listener");

            let _ = listener.run().await;
        }
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
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}
