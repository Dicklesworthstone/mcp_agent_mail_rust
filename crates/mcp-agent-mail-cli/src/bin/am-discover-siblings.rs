//! Explicit project discovery maintenance. Normal UI reads and agent
//! registration remain unchanged; this executable opts into the live write.

#![forbid(unsafe_code)]

use asupersync::{Outcome, runtime::RuntimeBuilder};
use clap::Parser;
use mcp_agent_mail_cli::{CliError, CliResult, context::AsyncCliContext};
use mcp_agent_mail_db::sibling_suggestions::{
    MAX_REFRESH_PAIRS, REFRESH_TTL_MICROS, refresh_project_sibling_suggestions,
};

#[derive(Debug, Parser)]
#[command(
    name = "am-discover-siblings",
    version,
    about = "Refresh sibling-project hints using local project and active agent-task metadata",
    after_help = "Uses DATABASE_URL and STORAGE_ROOT through the shared CLI write context.\n\
                  At most three suggestions are persisted per run. Existing unreviewed pairs\n\
                  have a twelve-hour refresh TTL. Confirmed and dismissed decisions are preserved.\n\
                  This does not link products, grant contact permissions, or send messages.\n\
                  The JSON result lists only suggestions persisted by this invocation."
)]
struct Args {
    /// Restrict discovery to this existing project, including older projects
    /// outside the newest-256-project candidate window.
    #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
    project_id: Option<i64>,
}

fn run(args: &Args) -> CliResult<serde_json::Value> {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .map_err(|error| CliError::Other(format!("discovery runtime: {error}")))?;
    runtime.block_on(async {
        let context = AsyncCliContext::open()?;
        let cx = asupersync::Cx::for_request();
        match refresh_project_sibling_suggestions(&cx, &context.pool, args.project_id).await {
            Outcome::Ok(summary) => Ok(serde_json::json!({
                "schema_version": 1,
                "project_id": args.project_id,
                "max_pairs_per_refresh": MAX_REFRESH_PAIRS,
                "refresh_ttl_micros": REFRESH_TTL_MICROS,
                "refresh": summary,
            })),
            Outcome::Err(error) => Err(CliError::Other(error.to_string())),
            Outcome::Cancelled(reason) => Err(CliError::Other(format!(
                "sibling discovery cancelled: {reason:?}"
            ))),
            Outcome::Panicked(panic) => Err(CliError::Other(format!(
                "sibling discovery failed: {panic:?}"
            ))),
        }
    })
}

fn main() {
    // Clap handles help, version, and invalid arguments before any DB open.
    let args = Args::parse();
    mcp_agent_mail_core::diagnostics::init_process_start();
    match run(&args).and_then(|report| {
        serde_json::to_string_pretty(&report).map_err(|error| CliError::Format(error.to_string()))
    }) {
        Ok(report) => println!("{report}"),
        Err(error) => {
            eprintln!("am-discover-siblings: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_global_or_positive_project_scope() {
        assert!(
            Args::try_parse_from(["am-discover-siblings"])
                .unwrap()
                .project_id
                .is_none()
        );
        assert_eq!(
            Args::try_parse_from(["am-discover-siblings", "--project-id", "42"])
                .unwrap()
                .project_id,
            Some(42)
        );
    }

    #[test]
    fn rejects_invalid_scope_and_unknown_arguments_before_database_access() {
        for invalid in ["0", "-1", "not-an-id"] {
            assert!(
                Args::try_parse_from(["am-discover-siblings", "--project-id", invalid]).is_err()
            );
        }
        assert!(Args::try_parse_from(["am-discover-siblings", "--broadcast"]).is_err());
    }
}
