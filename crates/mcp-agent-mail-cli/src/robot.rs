//! Agent-facing robot commands and output types.
//!
//! The public command surface stays here; the existing implementations live in
//! `commands`. Overview has a cold-process collector rather than an in-process
//! cache whose generation check repeats whole-mailbox work on every invocation.

#[path = "robot_commands.rs"]
mod commands;
mod overview;

pub use commands::*;

/// Execute a robot command, preserving the shared parser and output contract.
pub fn handle_robot(args: RobotArgs) -> Result<(), crate::CliError> {
    let RobotSubcommand::Overview { counts } = &args.command else {
        return commands::handle_robot(args);
    };
    let requested = args
        .format
        .or_else(|| args.json.then_some(OutputFormat::Json));
    if requested == Some(OutputFormat::Markdown) {
        // Keep the existing unsupported-format error and validate before any IO.
        return commands::handle_robot(args);
    }
    let format = OutputFormat::resolve(requested, false);
    let conn = crate::open_db_sync_robot()?;
    let out = if *counts {
        overview::build_counts_output(&conn, format)?
    } else {
        let projects = overview::build(&conn)?;
        overview::render(&projects, false, format)?
    };
    ftui_runtime::ftui_println!("{out}");
    Ok(())
}
