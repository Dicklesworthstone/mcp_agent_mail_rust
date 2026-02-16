//! Installed/active coding agent detection facade.
//!
//! With feature `agent-detect` enabled, this module re-exports detection primitives
//! from `franken-agent-detection`.
//! Without that feature, it preserves API shape and returns a deterministic
//! `FeatureDisabled` error.

#[cfg(feature = "agent-detect")]
pub use franken_agent_detection::{
    AgentDetectError, AgentDetectOptions, AgentDetectRootOverride, InstalledAgentDetectionEntry,
    InstalledAgentDetectionReport, InstalledAgentDetectionSummary, detect_installed_agents,
};

#[cfg(not(feature = "agent-detect"))]
use serde::{Deserialize, Serialize};
#[cfg(not(feature = "agent-detect"))]
use std::path::PathBuf;

#[cfg(not(feature = "agent-detect"))]
#[derive(Debug, Clone, Default)]
pub struct AgentDetectOptions {
    pub only_connectors: Option<Vec<String>>,
    pub include_undetected: bool,
    pub root_overrides: Vec<AgentDetectRootOverride>,
}

#[cfg(not(feature = "agent-detect"))]
#[derive(Debug, Clone)]
pub struct AgentDetectRootOverride {
    pub slug: String,
    pub root: PathBuf,
}

#[cfg(not(feature = "agent-detect"))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledAgentDetectionSummary {
    pub detected_count: usize,
    pub total_count: usize,
}

#[cfg(not(feature = "agent-detect"))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledAgentDetectionEntry {
    pub slug: String,
    pub detected: bool,
    pub evidence: Vec<String>,
    pub root_paths: Vec<String>,
}

#[cfg(not(feature = "agent-detect"))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledAgentDetectionReport {
    pub format_version: u32,
    pub generated_at: String,
    pub installed_agents: Vec<InstalledAgentDetectionEntry>,
    pub summary: InstalledAgentDetectionSummary,
}

#[cfg(not(feature = "agent-detect"))]
#[derive(Debug, thiserror::Error)]
pub enum AgentDetectError {
    #[error("agent detection is disabled (compile with feature `agent-detect`)")]
    FeatureDisabled,

    #[error("unknown connector(s): {connectors:?}")]
    UnknownConnectors { connectors: Vec<String> },
}

#[cfg(not(feature = "agent-detect"))]
#[allow(clippy::missing_const_for_fn)]
pub fn detect_installed_agents(
    opts: &AgentDetectOptions,
) -> Result<InstalledAgentDetectionReport, AgentDetectError> {
    let _ = opts;
    Err(AgentDetectError::FeatureDisabled)
}

#[cfg(all(test, not(feature = "agent-detect")))]
mod tests {
    use super::*;

    #[test]
    fn detect_installed_agents_returns_feature_disabled_error() {
        let err =
            detect_installed_agents(&AgentDetectOptions::default()).expect_err("expected error");
        assert!(matches!(err, AgentDetectError::FeatureDisabled));
    }
}
