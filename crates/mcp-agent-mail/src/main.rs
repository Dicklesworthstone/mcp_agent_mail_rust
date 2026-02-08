//! MCP Agent Mail - multi-agent coordination via MCP
//!
//! This is the main entry point for the MCP Agent Mail server.

#![forbid(unsafe_code)]

use std::io::IsTerminal;

use clap::{Parser, Subcommand, ValueEnum};
use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::config::{ConfigSource, InterfaceMode, InterfaceModeResolver, env_value};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "mcp-agent-mail")]
#[command(
    version,
    about = "MCP Agent Mail server (HTTP/MCP runtime + TUI)",
    after_help = "Operator CLI commands (guard/archive/share/etc) live in the `am` binary:\n  cargo run -p mcp-agent-mail-cli -- --help"
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

fn main() {
    // Initialize logging
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cli = Cli::parse();

    // Resolve interface mode (MCP binary → MCP default per ADR-001)
    let resolver = InterfaceModeResolver::new(InterfaceMode::Mcp);
    if let Some(warning) = resolver.validate() {
        tracing::warn!("{warning}");
    }
    let resolved_mode = resolver.resolve();
    tracing::debug!("Interface mode: {resolved_mode}");

    // Load configuration and stamp it with the resolved mode
    let mut config = Config::from_env();
    config.interface_mode = resolved_mode.mode;

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
         Agent Mail MCP server accepts: serve, config\n\
         For operator CLI commands, use: mcp-agent-mail-cli {command}"
    );

    // Show tip only when a TTY is detected (human, not agent)
    if std::io::stderr().is_terminal() {
        eprintln!("\nTip: Run `mcp-agent-mail-cli --help` for the full command list.");
    }
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
}
