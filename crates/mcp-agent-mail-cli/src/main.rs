#![forbid(unsafe_code)]

// GH#161: mimalloc global allocator so `am serve-http` does not retain multi-GB
// of glibc per-thread arena memory under sustained load. See the matching note
// in crates/mcp-agent-mail/src/main.rs. No `unsafe` needed to declare the static.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // Initialize process start time immediately for accurate uptime.
    mcp_agent_mail_core::diagnostics::init_process_start();
    std::process::exit(mcp_agent_mail_cli::run());
}
