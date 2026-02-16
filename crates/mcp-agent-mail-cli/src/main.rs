#![forbid(unsafe_code)]

fn main() {
    // Initialize process start time immediately for accurate uptime.
    mcp_agent_mail_core::diagnostics::init_process_start();
    std::process::exit(mcp_agent_mail_cli::run());
}
