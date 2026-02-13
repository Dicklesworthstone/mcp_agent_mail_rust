//! MCP Agent Mail - multi-agent coordination via MCP
//!
//! This is the main entry point for the MCP Agent Mail server.

#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::config::{ConfigSource, InterfaceMode, env_value};
use mcp_agent_mail_server::startup_checks::{self, PortStatus};
use tracing_subscriber::EnvFilter;

/// Runtime interface mode selector for the `mcp-agent-mail` binary.
///
/// Default is MCP. `AM_INTERFACE_MODE=cli` opts into routing the process to the CLI surface
/// (equivalent to the `am` binary). This is defined by ADR-002.
fn parse_am_interface_mode(raw: Option<&str>) -> Result<InterfaceMode, String> {
    let Some(raw) = raw else {
        return Ok(InterfaceMode::Mcp);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(InterfaceMode::Mcp);
    }
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "mcp" => Ok(InterfaceMode::Mcp),
        "cli" => Ok(InterfaceMode::Cli),
        other => Err(format!(
            "Invalid AM_INTERFACE_MODE={other:?} (expected \"mcp\" or \"cli\")"
        )),
    }
}

fn am_interface_mode_from_env() -> Result<InterfaceMode, String> {
    parse_am_interface_mode(env::var("AM_INTERFACE_MODE").ok().as_deref())
}

#[derive(Parser)]
#[command(name = "mcp-agent-mail")]
#[command(
    version,
    about = "MCP Agent Mail server (HTTP/MCP runtime + TUI)",
    after_help = "Operator CLI commands live in `am`:\n  am --help\n\nOr enable the CLI surface on this same binary:\n  AM_INTERFACE_MODE=cli mcp-agent-mail --help"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the MCP server (default)
    Serve {
        /// Host to bind to
        #[arg(long)]
        host: Option<String>,

        /// Port to bind to
        #[arg(long)]
        port: Option<u16>,

        /// Explicit MCP base path (`mcp`, `api`, `/custom/`).
        ///
        /// Takes precedence over `--transport` and `HTTP_PATH`.
        #[arg(long)]
        path: Option<String>,

        /// Transport preset for base-path selection.
        ///
        /// `auto` uses `HTTP_PATH` when present, otherwise defaults to `/mcp/`.
        #[arg(long, value_enum, default_value_t = ServeTransport::Auto)]
        transport: ServeTransport,

        /// Disable the interactive TUI (headless/CI mode).
        #[arg(long)]
        no_tui: bool,

        /// Read `HTTP_BEARER_TOKEN` fallback from this env file for `serve`.
        ///
        /// Process env and regular config loading still take precedence.
        #[arg(long)]
        env_file: Option<String>,

        /// Reuse a compatible already-running Agent Mail server on the same host/port.
        #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_reuse_running")]
        reuse_running: bool,

        /// Disable reuse checks and always attempt a fresh server start.
        #[arg(long, action = ArgAction::SetTrue, conflicts_with = "reuse_running")]
        no_reuse_running: bool,
    },

    /// Show configuration
    Config,

    /// Catch-all for unknown subcommands (denial gate per ADR-001)
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Commands accepted by the MCP server binary (per SPEC-meta-command-allowlist.md).
/// `--version` and `--help` are handled by clap before dispatch.
#[cfg(test)]
const MCP_ALLOWED_COMMANDS: &[&str] = &["serve", "config"];

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ServeTransport {
    Auto,
    Mcp,
    Api,
}

impl ServeTransport {
    const fn explicit_path(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Mcp => Some("/mcp/"),
            Self::Api => Some("/api/"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HttpPathSource {
    CliPath,
    CliTransport,
    EnvHttpPath,
    ServeDefault,
}

impl HttpPathSource {
    #[cfg(test)]
    const fn as_str(self) -> &'static str {
        match self {
            Self::CliPath => "--path",
            Self::CliTransport => "--transport",
            Self::EnvHttpPath => "HTTP_PATH",
            Self::ServeDefault => "serve-default",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedServeHttpPath {
    path: String,
    source: HttpPathSource,
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

fn resolve_serve_http_path(
    cli_path: Option<&str>,
    transport: ServeTransport,
    env_http_path: Option<String>,
) -> ResolvedServeHttpPath {
    if let Some(path) = cli_path {
        return ResolvedServeHttpPath {
            path: normalize_http_path(path),
            source: HttpPathSource::CliPath,
        };
    }

    if let Some(path) = transport.explicit_path() {
        return ResolvedServeHttpPath {
            path: normalize_http_path(path),
            source: HttpPathSource::CliTransport,
        };
    }

    if let Some(path) = env_http_path.filter(|v| !v.trim().is_empty()) {
        return ResolvedServeHttpPath {
            path: normalize_http_path(&path),
            source: HttpPathSource::EnvHttpPath,
        };
    }

    ResolvedServeHttpPath {
        path: "/mcp/".to_string(),
        source: HttpPathSource::ServeDefault,
    }
}

fn parse_reuse_running_env(raw: Option<&str>) -> bool {
    let Some(raw) = raw else {
        return true;
    };
    let normalized = raw.trim().to_ascii_lowercase();
    !matches!(normalized.as_str(), "0" | "false" | "no" | "off")
}

fn unquote_env_value(raw: &str) -> &str {
    if raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')))
    {
        &raw[1..raw.len() - 1]
    } else {
        raw
    }
}

fn load_env_file_value(path: &Path, key: &str) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    let mut matched: Option<Option<String>> = None;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let normalized = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let (lhs, rhs) = normalized.split_once('=')?;
        if lhs.trim() != key {
            continue;
        }
        let value = unquote_env_value(rhs.trim()).trim().to_string();
        if value.is_empty() {
            matched = Some(None);
            continue;
        }
        matched = Some(Some(value));
    }
    matched.flatten()
}

fn resolve_http_bearer_token_for_serve(
    current_token: Option<&str>,
    env_file: Option<&str>,
) -> Option<String> {
    if current_token.is_some_and(|token| !token.trim().is_empty()) {
        return None;
    }
    let path = env_file?;
    load_env_file_value(Path::new(path), "HTTP_BEARER_TOKEN")
}

fn resolve_reuse_running_setting(
    reuse_running_flag: bool,
    no_reuse_running_flag: bool,
    env_override: Option<&str>,
) -> bool {
    if reuse_running_flag {
        return true;
    }
    if no_reuse_running_flag {
        return false;
    }
    parse_reuse_running_env(env_override)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReusePreflightDecision {
    Proceed,
    ReusedExistingServer,
    AgentMailServerRunning,
    PortOccupiedByOtherProcess { description: String },
}

fn decide_reuse_preflight(
    reuse_running_enabled: bool,
    port_status: PortStatus,
) -> ReusePreflightDecision {
    if !reuse_running_enabled {
        return match port_status {
            PortStatus::AgentMailServer => ReusePreflightDecision::AgentMailServerRunning,
            PortStatus::OtherProcess { description } => {
                ReusePreflightDecision::PortOccupiedByOtherProcess { description }
            }
            PortStatus::Free | PortStatus::Error { .. } => ReusePreflightDecision::Proceed,
        };
    }

    match port_status {
        PortStatus::AgentMailServer => ReusePreflightDecision::ReusedExistingServer,
        PortStatus::OtherProcess { description } => {
            ReusePreflightDecision::PortOccupiedByOtherProcess { description }
        }
        PortStatus::Free | PortStatus::Error { .. } => ReusePreflightDecision::Proceed,
    }
}

#[allow(clippy::too_many_lines)]
fn main() {
    // Decide runtime mode before setting up logging or parsing the MCP CLI.
    // This ensures `--help` renders the correct surface and avoids polluting CLI-mode output.
    let mode = match am_interface_mode_from_env() {
        Ok(m) => m,
        Err(msg) => {
            eprintln!("Error: {msg}");
            eprintln!("Usage: AM_INTERFACE_MODE={{mcp|cli}} mcp-agent-mail ...");
            std::process::exit(2);
        }
    };

    if mode.is_cli() {
        let Some(cmd) = env::args().nth(1) else {
            render_cli_mode_missing_command();
            std::process::exit(2);
        };

        // Deterministic wrong-mode denial for MCP-only commands that users commonly try.
        //
        // Note: `config` is NOT denied because the CLI surface has its own `config` command.
        if cmd == "serve" {
            render_cli_mode_denial(&cmd);
            std::process::exit(2);
        }

        // Route to the CLI surface with the correct invocation name for help/usage.
        std::process::exit(mcp_agent_mail_cli::run_with_invocation_name(
            "mcp-agent-mail",
        ));
    }

    // MCP mode: initialize logging and proceed with the server binary behavior.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cli = Cli::parse();

    // Load configuration and stamp interface mode (binary-level, per ADR-001).
    let mut config = Config::from_env();
    config.interface_mode = InterfaceMode::Mcp;

    if cli.verbose {
        tracing::info!("Configuration loaded: {:?}", config);
    }

    match cli.command {
        None => {
            // Default: start MCP server in stdio mode
            tracing::info!("Starting MCP Agent Mail server (stdio mode)");
            mcp_agent_mail_server::run_stdio(&config);
        }
        Some(Commands::Serve {
            host,
            port,
            path,
            transport,
            no_tui,
            env_file,
            reuse_running,
            no_reuse_running,
        }) => {
            let mut config = config;
            let host_cli = host.is_some();
            let port_cli = port.is_some();
            if let Some(host) = host {
                config.http_host = host;
            }
            if let Some(port) = port {
                config.http_port = port;
            }
            if no_tui {
                config.tui_enabled = false;
            }
            let resolved_path =
                resolve_serve_http_path(path.as_deref(), transport, env_value("HTTP_PATH"));
            config.http_path = resolved_path.path;
            if let Some(token) = resolve_http_bearer_token_for_serve(
                config.http_bearer_token.as_deref(),
                env_file.as_deref(),
            ) {
                config.http_bearer_token = Some(token);
            }
            let reuse_running_enabled = resolve_reuse_running_setting(
                reuse_running,
                no_reuse_running,
                env_value("AM_REUSE_RUNNING").as_deref(),
            );

            let preflight_decision = decide_reuse_preflight(
                reuse_running_enabled,
                startup_checks::check_port_status(&config.http_host, config.http_port),
            );
            match preflight_decision {
                ReusePreflightDecision::Proceed => {}
                ReusePreflightDecision::ReusedExistingServer => {
                    eprintln!(
                        "am: reusing existing Agent Mail server on {}:{}.",
                        config.http_host, config.http_port
                    );
                    return;
                }
                ReusePreflightDecision::AgentMailServerRunning => {
                    eprintln!(
                        "am: an Agent Mail server is already running on {}:{}.",
                        config.http_host, config.http_port
                    );
                    eprintln!(
                        "am: use --reuse-running to reuse it, or stop it before starting a new instance."
                    );
                    std::process::exit(2);
                }
                ReusePreflightDecision::PortOccupiedByOtherProcess { description } => {
                    eprintln!(
                        "am: port {} is in use by a non-Agent-Mail process on {}.",
                        config.http_port, config.http_host
                    );
                    if !description.trim().is_empty() {
                        eprintln!("am: {description}");
                    }
                    eprintln!("am: free the port or choose a different one with --port.");
                    std::process::exit(2);
                }
            }

            // Build and display startup diagnostics
            let mut summary = config.bootstrap_summary();

            // CLI args override the auto-detected source for host/port/path.
            if host_cli {
                summary.set_source("host", ConfigSource::CliArg);
            }
            if port_cli {
                summary.set_source("port", ConfigSource::CliArg);
            }
            let path_source = match resolved_path.source {
                HttpPathSource::CliPath | HttpPathSource::CliTransport => ConfigSource::CliArg,
                HttpPathSource::EnvHttpPath => ConfigSource::ProcessEnv,
                HttpPathSource::ServeDefault => ConfigSource::Default,
            };
            summary.set("path", config.http_path.clone(), path_source);
            let mode = if config.tui_enabled && std::io::stdout().is_terminal() {
                "HTTP + TUI"
            } else {
                "HTTP (headless)"
            };
            eprintln!("{}", summary.format(mode));

            if let Err(err) = mcp_agent_mail_server::run_http_with_tui(&config) {
                tracing::error!("HTTP server failed: {err}");
                std::process::exit(1);
            }
        }
        Some(Commands::Config) => {
            // Show configuration
            ftui_runtime::ftui_println!("{:#?}", config);
        }
        Some(Commands::External(args)) => {
            // Denial gate (ADR-001 Invariant 4, SPEC-denial-ux-contract)
            let command = args.first().map_or("(unknown)", String::as_str);
            render_denial(command);
            std::process::exit(2);
        }
    }
}

/// MCP-mode denial renderer per SPEC-denial-ux-contract.md.
///
/// Prints a clear error to stderr explaining that the command belongs in the
/// CLI binary, with remediation hints.
fn render_denial(command: &str) {
    eprintln!(
        "Error: \"{command}\" is not an MCP server command.\n\n\
         Agent Mail is not a CLI.\n\
         Agent Mail MCP server accepts: serve, config\n\
         For operator CLI commands, use: am {command}\n\
         Or enable CLI mode: AM_INTERFACE_MODE=cli mcp-agent-mail {command} ..."
    );

    // Show tip only when a TTY is detected (human, not agent)
    let no_color = env::var_os("NO_COLOR").is_some();
    if std::io::stderr().is_terminal() && !no_color {
        eprintln!("\nTip: Run `am --help` for the full command list.");
    }
}

fn render_cli_mode_missing_command() {
    eprintln!(
        "Error: CLI mode is enabled (AM_INTERFACE_MODE=cli) but no subcommand was provided.\n\n\
         To run the CLI:\n\
           mcp-agent-mail --help\n\n\
         To start the MCP server:\n\
           unset AM_INTERFACE_MODE   # (or set AM_INTERFACE_MODE=mcp)\n\
           mcp-agent-mail serve ..."
    );
}

/// CLI-mode denial renderer for MCP-only commands.
///
/// CLI mode is enabled by `AM_INTERFACE_MODE=cli` (ADR-002, SPEC-interface-mode-switch).
fn render_cli_mode_denial(command: &str) {
    eprintln!(
        "Error: \"{command}\" is not available in CLI mode (AM_INTERFACE_MODE=cli).\n\n\
         To start the MCP server:\n\
           unset AM_INTERFACE_MODE   # (or set AM_INTERFACE_MODE=mcp)\n\
           mcp-agent-mail serve ...\n\n\
         CLI equivalents:\n\
           mcp-agent-mail serve-http ...\n\
           mcp-agent-mail serve-stdio ..."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_http_path_handles_presets_and_custom_paths() {
        assert_eq!(normalize_http_path("mcp"), "/mcp/");
        assert_eq!(normalize_http_path("/api"), "/api/");
        assert_eq!(normalize_http_path("/api///"), "/api/");
        assert_eq!(normalize_http_path("custom/v1"), "/custom/v1/");
        assert_eq!(normalize_http_path("/"), "/");
        assert_eq!(normalize_http_path(""), "/");
    }

    #[test]
    fn resolve_serve_http_path_prefers_cli_path_over_everything() {
        let resolved =
            resolve_serve_http_path(Some("/custom"), ServeTransport::Api, Some("/mcp/".into()));

        assert_eq!(resolved.path, "/custom/");
        assert_eq!(resolved.source, HttpPathSource::CliPath);
    }

    #[test]
    fn resolve_serve_http_path_uses_transport_when_path_not_provided() {
        let resolved =
            resolve_serve_http_path(None, ServeTransport::Api, Some("/mcp/".to_string()));

        assert_eq!(resolved.path, "/api/");
        assert_eq!(resolved.source, HttpPathSource::CliTransport);
    }

    #[test]
    fn resolve_serve_http_path_uses_env_when_auto_transport() {
        let resolved = resolve_serve_http_path(None, ServeTransport::Auto, Some("/api".into()));

        assert_eq!(resolved.path, "/api/");
        assert_eq!(resolved.source, HttpPathSource::EnvHttpPath);
    }

    #[test]
    fn resolve_serve_http_path_falls_back_to_mcp_default() {
        let resolved = resolve_serve_http_path(None, ServeTransport::Auto, None);

        assert_eq!(resolved.path, "/mcp/");
        assert_eq!(resolved.source, HttpPathSource::ServeDefault);
    }

    #[test]
    fn serve_command_no_tui_flag_parsed() {
        let cli = Cli::try_parse_from(["mcp-agent-mail", "serve", "--no-tui", "--host", "0.0.0.0"])
            .expect("should parse");

        match cli.command {
            Some(Commands::Serve { no_tui, host, .. }) => {
                assert!(no_tui);
                assert_eq!(host.as_deref(), Some("0.0.0.0"));
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn serve_command_defaults_tui_on() {
        let cli = Cli::try_parse_from(["mcp-agent-mail", "serve"]).expect("should parse");

        match cli.command {
            Some(Commands::Serve { no_tui, .. }) => {
                assert!(!no_tui);
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn serve_transport_explicit_path_values() {
        assert_eq!(ServeTransport::Auto.explicit_path(), None);
        assert_eq!(ServeTransport::Mcp.explicit_path(), Some("/mcp/"));
        assert_eq!(ServeTransport::Api.explicit_path(), Some("/api/"));
    }

    #[test]
    fn serve_command_reuse_flags_parse_and_conflict() {
        let cli = Cli::try_parse_from([
            "mcp-agent-mail",
            "serve",
            "--env-file",
            "/tmp/custom-agent-mail.env",
        ])
        .expect("should parse env-file");
        match cli.command {
            Some(Commands::Serve { env_file, .. }) => {
                assert_eq!(env_file.as_deref(), Some("/tmp/custom-agent-mail.env"));
            }
            other => panic!("expected Serve, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["mcp-agent-mail", "serve", "--no-reuse-running"])
            .expect("should parse");
        match cli.command {
            Some(Commands::Serve {
                reuse_running,
                no_reuse_running,
                ..
            }) => {
                assert!(!reuse_running);
                assert!(no_reuse_running);
            }
            other => panic!("expected Serve, got {other:?}"),
        }

        let Err(err) = Cli::try_parse_from([
            "mcp-agent-mail",
            "serve",
            "--reuse-running",
            "--no-reuse-running",
        ]) else {
            panic!("conflicting flags should fail")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parse_reuse_running_env_defaults_to_true() {
        assert!(parse_reuse_running_env(None));
        assert!(parse_reuse_running_env(Some("")));
        assert!(parse_reuse_running_env(Some("1")));
        assert!(parse_reuse_running_env(Some("true")));
        assert!(parse_reuse_running_env(Some("yes")));
        assert!(parse_reuse_running_env(Some("on")));
    }

    #[test]
    fn parse_reuse_running_env_handles_falsey_values() {
        assert!(!parse_reuse_running_env(Some("0")));
        assert!(!parse_reuse_running_env(Some("false")));
        assert!(!parse_reuse_running_env(Some("no")));
        assert!(!parse_reuse_running_env(Some("off")));
    }

    #[test]
    fn resolve_reuse_running_setting_prioritizes_cli_flags() {
        assert!(resolve_reuse_running_setting(true, false, Some("0")));
        assert!(!resolve_reuse_running_setting(false, true, Some("1")));
        assert!(resolve_reuse_running_setting(false, false, Some("1")));
        assert!(!resolve_reuse_running_setting(false, false, Some("0")));
    }

    #[test]
    fn decide_reuse_preflight_maps_port_status() {
        assert_eq!(
            decide_reuse_preflight(
                false,
                PortStatus::OtherProcess {
                    description: "x".to_string()
                }
            ),
            ReusePreflightDecision::PortOccupiedByOtherProcess {
                description: "x".to_string(),
            }
        );
        assert_eq!(
            decide_reuse_preflight(false, PortStatus::AgentMailServer),
            ReusePreflightDecision::AgentMailServerRunning
        );
        assert_eq!(
            decide_reuse_preflight(
                false,
                PortStatus::Error {
                    kind: std::io::ErrorKind::PermissionDenied,
                    message: "denied".to_string(),
                },
            ),
            ReusePreflightDecision::Proceed
        );

        assert_eq!(
            decide_reuse_preflight(true, PortStatus::Free),
            ReusePreflightDecision::Proceed
        );
        assert_eq!(
            decide_reuse_preflight(true, PortStatus::AgentMailServer),
            ReusePreflightDecision::ReusedExistingServer
        );
        assert_eq!(
            decide_reuse_preflight(
                true,
                PortStatus::OtherProcess {
                    description: "other".to_string(),
                },
            ),
            ReusePreflightDecision::PortOccupiedByOtherProcess {
                description: "other".to_string(),
            }
        );
        assert_eq!(
            decide_reuse_preflight(
                true,
                PortStatus::Error {
                    kind: std::io::ErrorKind::PermissionDenied,
                    message: "denied".to_string(),
                },
            ),
            ReusePreflightDecision::Proceed
        );
    }

    #[test]
    fn resolve_http_bearer_token_for_serve_loads_from_custom_env_file() {
        let tmp = tempfile::NamedTempFile::new().expect("temp file");
        fs::write(
            tmp.path(),
            "export HTTP_BEARER_TOKEN='token-from-custom-env'\n",
        )
        .expect("write env file");
        let resolved = resolve_http_bearer_token_for_serve(None, tmp.path().to_str());
        assert_eq!(resolved.as_deref(), Some("token-from-custom-env"));
    }

    #[test]
    fn resolve_http_bearer_token_for_serve_does_not_override_existing_token() {
        let tmp = tempfile::NamedTempFile::new().expect("temp file");
        fs::write(tmp.path(), "HTTP_BEARER_TOKEN=token-from-file\n").expect("write env file");
        let resolved =
            resolve_http_bearer_token_for_serve(Some("from-process-env"), tmp.path().to_str());
        assert!(resolved.is_none());
    }

    #[test]
    fn load_env_file_value_handles_double_quoted_values() {
        let tmp = tempfile::NamedTempFile::new().expect("temp file");
        fs::write(tmp.path(), "HTTP_BEARER_TOKEN=\"quoted-token\"\n").expect("write env file");
        let token = load_env_file_value(tmp.path(), "HTTP_BEARER_TOKEN");
        assert_eq!(token.as_deref(), Some("quoted-token"));
    }

    #[test]
    fn load_env_file_value_last_match_wins() {
        let tmp = tempfile::NamedTempFile::new().expect("temp file");
        fs::write(
            tmp.path(),
            "HTTP_BEARER_TOKEN=first-token\nHTTP_BEARER_TOKEN=second-token\n",
        )
        .expect("write env file");
        let token = load_env_file_value(tmp.path(), "HTTP_BEARER_TOKEN");
        assert_eq!(token.as_deref(), Some("second-token"));
    }

    #[test]
    fn load_env_file_value_handles_export_and_whitespace() {
        let tmp = tempfile::NamedTempFile::new().expect("temp file");
        fs::write(
            tmp.path(),
            "  export HTTP_BEARER_TOKEN =   'trimmed-token'   \n",
        )
        .expect("write env file");
        let token = load_env_file_value(tmp.path(), "HTTP_BEARER_TOKEN");
        assert_eq!(token.as_deref(), Some("trimmed-token"));
    }

    #[test]
    fn load_env_file_value_treats_empty_last_value_as_absent() {
        let tmp = tempfile::NamedTempFile::new().expect("temp file");
        fs::write(
            tmp.path(),
            "HTTP_BEARER_TOKEN=present\nHTTP_BEARER_TOKEN=\n",
        )
        .expect("write env file");
        let token = load_env_file_value(tmp.path(), "HTTP_BEARER_TOKEN");
        assert!(token.is_none());
    }

    #[test]
    fn resolve_http_bearer_token_for_serve_handles_missing_file() {
        let missing = "/tmp/this-file-should-not-exist-agent-mail.env";
        let resolved = resolve_http_bearer_token_for_serve(None, Some(missing));
        assert!(resolved.is_none());
    }

    #[test]
    fn http_path_source_as_str_values() {
        assert_eq!(HttpPathSource::CliPath.as_str(), "--path");
        assert_eq!(HttpPathSource::CliTransport.as_str(), "--transport");
        assert_eq!(HttpPathSource::EnvHttpPath.as_str(), "HTTP_PATH");
        assert_eq!(HttpPathSource::ServeDefault.as_str(), "serve-default");
    }

    // -- Denial gate tests (br-21gj.3.1, br-21gj.3.4) --

    #[test]
    fn unknown_subcommand_parsed_as_external() {
        let cli = Cli::try_parse_from(["mcp-agent-mail", "share", "export"]).expect("should parse");

        match cli.command {
            Some(Commands::External(args)) => {
                assert_eq!(args[0], "share");
                assert_eq!(args[1], "export");
            }
            other => panic!("expected External, got {other:?}"),
        }
    }

    #[test]
    fn known_cli_commands_caught_by_external_gate() {
        for cmd in &["share", "guard", "doctor", "archive", "migrate"] {
            let cli = Cli::try_parse_from(["mcp-agent-mail", cmd]).expect("should parse");
            assert!(
                matches!(cli.command, Some(Commands::External(_))),
                "{cmd} should be caught as External"
            );
        }
    }

    #[test]
    fn allowed_commands_not_caught_by_external_gate() {
        for cmd in MCP_ALLOWED_COMMANDS {
            let cli = Cli::try_parse_from(["mcp-agent-mail", cmd]).expect("should parse");
            assert!(
                !matches!(cli.command, Some(Commands::External(_))),
                "{cmd} should NOT be caught as External"
            );
        }
    }

    #[test]
    fn no_subcommand_is_none() {
        let cli = Cli::try_parse_from(["mcp-agent-mail"]).expect("should parse");
        assert!(cli.command.is_none());
    }

    #[test]
    fn parse_am_interface_mode_defaults_to_mcp() {
        assert_eq!(parse_am_interface_mode(None).unwrap(), InterfaceMode::Mcp);
        assert_eq!(
            parse_am_interface_mode(Some("")).unwrap(),
            InterfaceMode::Mcp
        );
        assert_eq!(
            parse_am_interface_mode(Some("   ")).unwrap(),
            InterfaceMode::Mcp
        );
        assert_eq!(
            parse_am_interface_mode(Some("mcp")).unwrap(),
            InterfaceMode::Mcp
        );
        assert_eq!(
            parse_am_interface_mode(Some("MCP")).unwrap(),
            InterfaceMode::Mcp
        );
    }

    #[test]
    fn parse_am_interface_mode_parses_cli() {
        assert_eq!(
            parse_am_interface_mode(Some("cli")).unwrap(),
            InterfaceMode::Cli
        );
        assert_eq!(
            parse_am_interface_mode(Some(" CLI ")).unwrap(),
            InterfaceMode::Cli
        );
    }

    #[test]
    fn parse_am_interface_mode_rejects_invalid_values() {
        let err = parse_am_interface_mode(Some("wat")).unwrap_err();
        assert!(err.contains("AM_INTERFACE_MODE"));
        assert!(err.contains("mcp"));
        assert!(err.contains("cli"));
    }
}
