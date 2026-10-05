//! Mailbox maintenance worker lifecycle and read-only artifact reports.
//!
//! Archive convergence/retention, durable closeout replay and missing active
//! lease repair have independent workers. Disabling destructive retention does
//! not strand closeouts or leave the guard blind to a lost lease artifact.

#![forbid(unsafe_code)]

mod active;
mod archive;
mod pending;

pub use archive::{
    ArtifactDiskReport, ArtifactRetentionReport, ArtifactRetentionTotals, ArtifactRetentionWarning,
    ArtifactRootReport, anchor_settled_writes, artifact_retention_report,
};

/// Start archive maintenance, durable-intent replay and active lease repair.
/// Repeated starts do not create additional workers.
pub fn start(config: &mcp_agent_mail_core::Config) {
    archive::start(config);
    pending::start(config);
    active::start(config);
}

/// Stop and join all maintenance workers before closing the mailbox.
pub fn shutdown() {
    active::shutdown();
    pending::shutdown();
    archive::shutdown();
}
