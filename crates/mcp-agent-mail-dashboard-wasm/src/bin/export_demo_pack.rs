//! Export aggregate-only mailbox counts into a sanitized public replay pack.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use clap::Parser;
use mcp_agent_mail_dashboard_wasm::demo_pack::{DemoOperation, curated_public_demo};
use mcp_agent_mail_dashboard_wasm::tui_events::{DbStatSnapshot, MailEvent};
use sqlmodel_sqlite::SqliteConnection;

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

#[derive(Debug, Clone, Copy)]
struct AggregateCounts {
    projects: u64,
    agents: u64,
    messages: u64,
    file_reservations: u64,
    contact_links: u64,
    ack_pending: u64,
}

fn count(connection: &SqliteConnection, sql: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let rows = connection.query_sync(sql, &[])?;
    let value = rows
        .first()
        .ok_or("aggregate count query returned no row")?
        .get_named::<i64>("c")?;
    Ok(u64::try_from(value)?)
}

fn read_aggregates(
    connection: &SqliteConnection,
) -> Result<AggregateCounts, Box<dyn std::error::Error>> {
    Ok(AggregateCounts {
        projects: count(connection, "SELECT COUNT(*) AS c FROM projects")?,
        agents: count(connection, "SELECT COUNT(*) AS c FROM agents")?,
        messages: count(connection, "SELECT COUNT(*) AS c FROM messages")?,
        file_reservations: count(
            connection,
            "SELECT COUNT(*) AS c FROM file_reservations \
             WHERE released_ts IS NULL \
               AND expires_ts > CAST(strftime('%s', 'now') AS INTEGER) * 1000000",
        )?,
        contact_links: count(connection, "SELECT COUNT(*) AS c FROM agent_links")?,
        ack_pending: count(
            connection,
            "SELECT COUNT(*) AS c FROM message_recipients mr \
             JOIN messages m ON m.id = mr.message_id \
             WHERE m.ack_required = 1 AND mr.ack_ts IS NULL",
        )?,
    })
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
    let connection = SqliteConnection::open_file(source.to_string_lossy().into_owned())?;
    let counts = read_aggregates(&connection)?;

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
