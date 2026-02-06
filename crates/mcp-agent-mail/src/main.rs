//! MCP Agent Mail - multi-agent coordination via MCP
//!
//! This is the main entry point for the MCP Agent Mail server.

#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use mcp_agent_mail_core::Config;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "mcp-agent-mail")]
#[command(version, about = "MCP Agent Mail - multi-agent coordination via MCP")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server (default)
    Serve {
        /// Host to bind to
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// Port to bind to
        #[arg(long, default_value = "8765")]
        port: u16,
    },

    /// Guard commands (pre-commit hooks)
    Guard {
        #[command(subcommand)]
        action: GuardAction,
    },

    /// File reservation commands
    #[command(name = "file-reservations")]
    FileReservations {
        #[command(subcommand)]
        action: FileReservationAction,
    },

    /// Acknowledgement commands
    Acks {
        #[command(subcommand)]
        action: AckAction,
    },

    /// Share/export commands
    Share {
        #[command(subcommand)]
        action: ShareAction,
    },

    /// Archive commands
    Archive {
        #[command(subcommand)]
        action: ArchiveAction,
    },

    /// Mail commands
    Mail {
        #[command(subcommand)]
        action: MailAction,
    },

    /// Project commands
    Projects {
        #[command(subcommand)]
        action: ProjectAction,
    },

    /// Product commands
    Products {
        #[command(subcommand)]
        action: ProductAction,
    },

    /// Doctor commands (diagnostics)
    Doctor {
        #[command(subcommand)]
        action: DoctorAction,
    },

    /// Show configuration
    Config,
}

#[derive(Subcommand)]
enum GuardAction {
    /// Install pre-commit guard
    Install {
        project: String,
        code_repo_path: String,
    },
    /// Uninstall pre-commit guard
    Uninstall { code_repo_path: String },
}

#[derive(Subcommand)]
enum FileReservationAction {
    /// List file reservations
    List { project: String },
    /// Show conflicts
    Conflicts { project: String },
}

#[derive(Subcommand)]
enum AckAction {
    /// List pending acknowledgements
    Pending { project: String, agent: String },
    /// List overdue acknowledgements
    Overdue {
        project: String,
        agent: String,
        #[arg(long, default_value = "30")]
        ttl_minutes: u64,
    },
}

#[derive(Subcommand)]
enum ShareAction {
    /// Export for static hosting
    Export { db_path: String, output_dir: String },
    /// Create archive bundle
    Bundle { project: String },
}

#[derive(Subcommand)]
enum ArchiveAction {
    /// Create disaster recovery bundle
    Create { project: String },
    /// Restore from bundle
    Restore { bundle_path: String },
}

#[derive(Subcommand)]
enum MailAction {
    /// View inbox
    Inbox { project: String, agent: String },
    /// Send message
    Send {
        project: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
    },
}

#[derive(Subcommand)]
enum ProjectAction {
    /// List all projects
    List,
    /// Garbage collection
    Gc { project: String },
}

#[derive(Subcommand)]
enum ProductAction {
    /// Ensure product exists
    Ensure { product_key: String },
    /// Link projects to product
    Link {
        product_key: String,
        project_keys: Vec<String>,
    },
}

#[derive(Subcommand)]
enum DoctorAction {
    /// Check project health
    Check { project: String },
    /// Repair project
    Repair { project: String },
}

fn main() {
    // Initialize logging
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cli = Cli::parse();

    // Load configuration
    let config = Config::from_env();

    if cli.verbose {
        tracing::info!("Configuration loaded: {:?}", config);
    }

    match cli.command {
        None => {
            // Default: start MCP server in stdio mode
            tracing::info!("Starting MCP Agent Mail server (stdio mode)");
            mcp_agent_mail_server::run_stdio(&config);
        }
        Some(Commands::Serve { host, port }) => {
            // Start MCP server in HTTP mode (Streamable HTTP)
            tracing::info!("Starting MCP Agent Mail server (HTTP mode)");
            let mut config = config;
            config.http_host = host;
            config.http_port = port;
            if let Err(err) = mcp_agent_mail_server::run_http(&config) {
                tracing::error!("HTTP server failed: {err}");
                std::process::exit(1);
            }
        }
        Some(Commands::Config) => {
            // Show configuration
            ftui_runtime::ftui_println!("{:#?}", config);
        }
        Some(cmd) => {
            let (exit_code, message) = unsupported_command_response(&cmd);
            ftui_runtime::ftui_eprintln!("{message}");
            std::process::exit(exit_code);
        }
    }
}

fn unsupported_command_response(cmd: &Commands) -> (i32, String) {
    (
        2,
        format!(
            "Command {cmd:?} is not implemented in the `mcp-agent-mail` binary.\n\
Use the full CLI instead:\n\
  am <command> ...\n\
  cargo run -p mcp-agent-mail-cli -- <command> ..."
        ),
    )
}

impl std::fmt::Debug for Commands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serve { host, port } => write!(f, "Serve {{ host: {host}, port: {port} }}"),
            Self::Guard { .. } => write!(f, "Guard"),
            Self::FileReservations { .. } => write!(f, "FileReservations"),
            Self::Acks { .. } => write!(f, "Acks"),
            Self::Share { .. } => write!(f, "Share"),
            Self::Archive { .. } => write!(f, "Archive"),
            Self::Mail { .. } => write!(f, "Mail"),
            Self::Projects { .. } => write!(f, "Projects"),
            Self::Products { .. } => write!(f, "Products"),
            Self::Doctor { .. } => write!(f, "Doctor"),
            Self::Config => write!(f, "Config"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_command_response_is_nonzero_with_cli_guidance() {
        let cmd = Commands::Guard {
            action: GuardAction::Install {
                project: "my-project".to_string(),
                code_repo_path: "/tmp/repo".to_string(),
            },
        };

        let (exit_code, message) = unsupported_command_response(&cmd);

        assert_eq!(exit_code, 2);
        assert!(message.contains("not implemented"));
        assert!(message.contains("mcp-agent-mail-cli"));
        assert!(message.contains("am <command> ..."));
    }
}
