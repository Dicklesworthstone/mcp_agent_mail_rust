//! Export aggregate-only mailbox counts into a sanitized public replay pack.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use clap::Parser;
use mcp_agent_mail_dashboard_wasm::demo_pack::{DemoOperation, curated_public_demo};
use mcp_agent_mail_dashboard_wasm::exporter::{
    AggregateCounts, open_source_read_only, read_aggregates_snapshot,
};
use mcp_agent_mail_dashboard_wasm::tui_events::{DbStatSnapshot, MailEvent};

#[derive(Debug, Parser)]
#[command(
    name = "am-export-dashboard-demo",
    about = "Export privacy-bounded aggregate Agent Mail dashboard replay data"
)]
struct Args {
    /// Source Agent Mail SQLite database. Its path is never written to output.
    #[arg(long)]
    source: PathBuf,
    /// New output JSON path. The exporter refuses to overwrite existing files.
    #[arg(long)]
    output: PathBuf,
    /// Public source revision or deployment identifier.
    #[arg(long)]
    source_revision: String,
    /// Public ISO-8601 capture timestamp supplied by the release process.
    #[arg(long)]
    captured_at: String,
}

fn apply_aggregates(
    snapshot: &mut DbStatSnapshot,
    counts: AggregateCounts,
    message_delta: u64,
    ack_delta: i64,
    reservation_delta: i64,
) {
    snapshot.projects = counts.projects;
    snapshot.agents = counts.agents;
    snapshot.messages = counts.messages.saturating_add(message_delta);
    snapshot.file_reservations = counts
        .file_reservations
        .saturating_add_signed(reservation_delta);
    snapshot.contact_links = counts.contact_links;
    snapshot.ack_pending = counts.ack_pending.saturating_add_signed(ack_delta);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let source = args.source.canonicalize()?;
    if !source.is_file() {
        return Err("source must be a regular SQLite file".into());
    }
    // Read-only, fail-closed open plus a single-snapshot aggregate read: the
    // exporter never holds write capability on the private source, and all
    // six published counts describe one database state (br-h44pp).
    let connection = open_source_read_only(&source.to_string_lossy())?;
    let counts = read_aggregates_snapshot(&connection)?;

    let mut pack = curated_public_demo();
    pack.provenance.source_label =
        "aggregate counts exported from Agent Mail SQLite; all details synthetic".to_string();
    pack.provenance.source_revision = args.source_revision;
    pack.provenance.captured_at = args.captured_at;
    apply_aggregates(&mut pack.bootstrap.db_stats, counts, 0, 0, 0);

    for action in &mut pack.actions {
        match &mut action.operation {
            DemoOperation::SetDbStats { snapshot } => {
                let message_delta = u64::from(action.at_ms >= 2_000);
                let ack_delta = if action.at_ms < 2_000 {
                    0
                } else if action.at_ms < 6_000 {
                    1
                } else {
                    0
                };
                let reservation_delta = if action.at_ms >= 10_000 { -1 } else { 0 };
                apply_aggregates(
                    snapshot,
                    counts,
                    message_delta,
                    ack_delta,
                    reservation_delta,
                );
            }
            DemoOperation::PublishEvent {
                event: MailEvent::HealthPulse { db_stats, .. },
            } => apply_aggregates(db_stats, counts, 1, 0, -1),
            _ => {}
        }
    }

    pack.finalize_digest();
    pack.validate()?;
    let json = pack.to_pretty_json()?;

    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.output)?;
    output.write_all(json.as_bytes())?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    Ok(())
}
