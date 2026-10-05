//! Mailbox maintenance worker lifecycle and read-only artifact reports.
//!
//! Archive convergence/retention and durable closeout replay have independent
//! workers: disabling retention must not strand acknowledgements, and a slow
//! archive scan must not prevent a recovered database from accepting them.

#![forbid(unsafe_code)]

mod archive;
mod pending;

pub use archive::{
    ArtifactDiskReport, ArtifactRetentionReport, ArtifactRetentionTotals,
    ArtifactRetentionWarning, ArtifactRootReport, anchor_settled_writes,
    artifact_retention_report,
};

/// Start archive maintenance and durable-intent replay when applicable.
/// Repeated starts do not create additional workers.
pub fn start(config: &mcp_agent_mail_core::Config) {
    archive::start(config);
    pending::start(config);
}

/// Stop and join both maintenance workers before closing the mailbox.
pub fn shutdown() {
    pending::shutdown();
    archive::shutdown();
}
