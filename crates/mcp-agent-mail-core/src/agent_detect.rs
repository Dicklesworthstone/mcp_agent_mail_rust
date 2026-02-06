//! Installed/active coding agent detection (optional).
//!
//! This module integrates with the local `/dp/coding_agent_session_search` crate
//! (workspace dep `coding-agent-search`) when the `agent-detect` feature is enabled.
//!
//! The API is intentionally synchronous (tokio-free surface). Under the hood, the
//! upstream crate may depend on tokio for other features, but connector `detect()`
//! is a lightweight filesystem probe.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct AgentDetectOptions {
    /// Restrict detection to specific connector slugs (e.g. `["codex", "gemini"]`).
    ///
    /// When `None`, all known connectors are evaluated.
    pub only_connectors: Option<Vec<String>>,

    /// When false, omit entries that were not detected.
    pub include_undetected: bool,

    /// Optional per-connector root overrides for deterministic detection (tests/fixtures).
    ///
    /// If an override is provided for a connector slug, we do NOT call the upstream
    /// connector's `detect()`; instead we treat the connector as detected when any
    /// override root exists on disk.
    ///
    /// Rationale: Rust 2024 makes `std::env::set_var` unsafe and this workspace forbids
    /// `unsafe_code`, so tests cannot safely mutate process-wide environment variables.
    pub root_overrides: Vec<AgentDetectRootOverride>,
}

#[derive(Debug, Clone)]
pub struct AgentDetectRootOverride {
    pub slug: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledAgentDetectionSummary {
    pub detected_count: usize,
    pub total_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledAgentDetectionEntry {
    /// Stable connector/agent identifier (e.g. `codex`, `claude`, `gemini`).
    pub slug: String,
    pub detected: bool,
    pub evidence: Vec<String>,
    pub root_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledAgentDetectionReport {
    pub format_version: u32,
    pub generated_at: String,
    pub installed_agents: Vec<InstalledAgentDetectionEntry>,
    pub summary: InstalledAgentDetectionSummary,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentDetectError {
    #[error("agent detection is disabled (compile with feature `agent-detect`)")]
    FeatureDisabled,

    #[error("unknown connector(s): {connectors:?}")]
    UnknownConnectors { connectors: Vec<String> },
}

#[cfg(feature = "agent-detect")]
use std::collections::{HashMap, HashSet};

#[cfg(feature = "agent-detect")]
fn normalize_slug(s: &str) -> Option<String> {
    let slug = s.trim().to_ascii_lowercase();
    if slug.is_empty() { None } else { Some(slug) }
}

#[cfg(feature = "agent-detect")]
fn build_overrides_map(overrides: &[AgentDetectRootOverride]) -> HashMap<String, Vec<PathBuf>> {
    let mut out: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for o in overrides {
        let Some(slug) = normalize_slug(&o.slug) else {
            continue;
        };
        out.entry(slug).or_default().push(o.root.clone());
    }
    out
}

#[cfg(feature = "agent-detect")]
fn validate_known_connectors(
    available: &HashSet<&'static str>,
    only: &Option<HashSet<String>>,
    overrides: &HashMap<String, Vec<PathBuf>>,
) -> Result<(), AgentDetectError> {
    let mut unknown: Vec<String> = Vec::new();
    if let Some(only) = only {
        unknown.extend(
            only.iter()
                .filter(|slug| !available.contains(slug.as_str()))
                .cloned(),
        );
    }
    unknown.extend(
        overrides
            .keys()
            .filter(|slug| !available.contains(slug.as_str()))
            .cloned(),
    );
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort();
    unknown.dedup();
    Err(AgentDetectError::UnknownConnectors {
        connectors: unknown,
    })
}

#[cfg(feature = "agent-detect")]
fn entry_from_override(slug: &'static str, roots: &[PathBuf]) -> InstalledAgentDetectionEntry {
    let mut detected = false;
    let mut evidence: Vec<String> = Vec::new();
    let mut root_paths: Vec<String> = Vec::new();
    for root in roots {
        let root_str = root.display().to_string();
        if root.exists() {
            detected = true;
            root_paths.push(root_str.clone());
            evidence.push(format!("override root exists: {root_str}"));
        } else {
            evidence.push(format!("override root missing: {root_str}"));
        }
    }
    root_paths.sort();
    InstalledAgentDetectionEntry {
        slug: slug.to_string(),
        detected,
        evidence,
        root_paths,
    }
}

#[cfg(feature = "agent-detect")]
fn entry_from_detect(
    slug: &'static str,
    factory: fn() -> Box<dyn coding_agent_search::connectors::Connector + Send>,
) -> InstalledAgentDetectionEntry {
    let conn = factory();
    let detect = conn.detect();
    let mut root_paths: Vec<String> = detect
        .root_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    root_paths.sort();
    InstalledAgentDetectionEntry {
        slug: slug.to_string(),
        detected: detect.detected,
        evidence: detect.evidence,
        root_paths,
    }
}

#[cfg(feature = "agent-detect")]
fn detect_installed_agents_enabled(
    opts: &AgentDetectOptions,
) -> Result<InstalledAgentDetectionReport, AgentDetectError> {
    use coding_agent_search::indexer::get_connector_factories;

    let factories = get_connector_factories();
    let available: HashSet<&'static str> = factories.iter().map(|(s, _)| *s).collect();
    let overrides = build_overrides_map(&opts.root_overrides);

    let only: Option<HashSet<String>> = opts
        .only_connectors
        .as_ref()
        .map(|slugs| slugs.iter().filter_map(|s| normalize_slug(s)).collect());

    validate_known_connectors(&available, &only, &overrides)?;

    let mut all_entries: Vec<InstalledAgentDetectionEntry> = factories
        .into_iter()
        .filter(|(slug, _)| only.as_ref().is_none_or(|set| set.contains(*slug)))
        .map(|(slug, factory)| {
            overrides.get(slug).map_or_else(
                || entry_from_detect(slug, factory),
                |roots| entry_from_override(slug, roots),
            )
        })
        .collect();

    all_entries.sort_by(|a, b| a.slug.cmp(&b.slug));

    let detected_count = all_entries.iter().filter(|e| e.detected).count();
    let total_count = all_entries.len();

    Ok(InstalledAgentDetectionReport {
        format_version: 1,
        generated_at: chrono::Utc::now().to_rfc3339(),
        installed_agents: if opts.include_undetected {
            all_entries
        } else {
            all_entries.into_iter().filter(|e| e.detected).collect()
        },
        summary: InstalledAgentDetectionSummary {
            detected_count,
            total_count,
        },
    })
}

/// Detect installed/available coding agents by running connector `detect()` probes.
///
/// This returns a stable JSON shape (via `serde`) intended for CLI/resource consumption.
///
/// # Errors
/// - Returns [`AgentDetectError::FeatureDisabled`] when compiled without `agent-detect`.
/// - Returns [`AgentDetectError::UnknownConnectors`] when `only_connectors` includes unknown slugs.
#[allow(clippy::missing_const_for_fn)]
pub fn detect_installed_agents(
    opts: &AgentDetectOptions,
) -> Result<InstalledAgentDetectionReport, AgentDetectError> {
    #[cfg(not(feature = "agent-detect"))]
    {
        let _ = opts;
        Err(AgentDetectError::FeatureDisabled)
    }

    #[cfg(feature = "agent-detect")]
    {
        detect_installed_agents_enabled(opts)
    }
}

#[cfg(all(test, not(feature = "agent-detect")))]
mod feature_disabled_tests {
    use super::*;

    #[test]
    fn detect_installed_agents_returns_feature_disabled_error() {
        let err =
            detect_installed_agents(&AgentDetectOptions::default()).expect_err("expected error");
        assert!(matches!(err, AgentDetectError::FeatureDisabled));
    }
}

#[cfg(all(test, feature = "agent-detect"))]
mod tests {
    use super::*;

    #[test]
    fn detect_installed_agents_can_be_scoped_to_specific_connectors() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let codex_root = tmp.path().join("codex-home").join("sessions");
        std::fs::create_dir_all(&codex_root).expect("create codex sessions");

        let gemini_root = tmp.path().join("gemini-home").join("tmp");
        std::fs::create_dir_all(&gemini_root).expect("create gemini root");

        let report = detect_installed_agents(&AgentDetectOptions {
            only_connectors: Some(vec!["codex".to_string(), "gemini".to_string()]),
            include_undetected: true,
            root_overrides: vec![
                AgentDetectRootOverride {
                    slug: "codex".to_string(),
                    root: codex_root.clone(),
                },
                AgentDetectRootOverride {
                    slug: "gemini".to_string(),
                    root: gemini_root.clone(),
                },
            ],
        })
        .expect("detect");

        assert_eq!(report.format_version, 1);
        assert!(!report.generated_at.is_empty());
        assert_eq!(report.summary.total_count, 2);
        assert_eq!(report.summary.detected_count, 2);

        let slugs: Vec<&str> = report
            .installed_agents
            .iter()
            .map(|e| e.slug.as_str())
            .collect();
        assert_eq!(slugs, vec!["codex", "gemini"]);

        let codex = report
            .installed_agents
            .iter()
            .find(|e| e.slug == "codex")
            .expect("codex entry");
        assert!(codex.detected);
        assert!(codex.root_paths.iter().any(|p| p.ends_with("/sessions")));

        let gemini = report
            .installed_agents
            .iter()
            .find(|e| e.slug == "gemini")
            .expect("gemini entry");
        assert!(gemini.detected);
        assert_eq!(gemini.root_paths, vec![gemini_root.display().to_string()]);
    }

    #[test]
    fn unknown_connectors_are_rejected() {
        let err = detect_installed_agents(&AgentDetectOptions {
            only_connectors: Some(vec!["not-a-real-connector".to_string()]),
            include_undetected: true,
            root_overrides: vec![],
        })
        .expect_err("should error");

        match err {
            AgentDetectError::UnknownConnectors { connectors } => {
                assert_eq!(connectors, vec!["not-a-real-connector".to_string()]);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn unknown_overrides_are_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = detect_installed_agents(&AgentDetectOptions {
            only_connectors: Some(vec!["codex".to_string()]),
            include_undetected: true,
            root_overrides: vec![AgentDetectRootOverride {
                slug: "definitely-unknown".to_string(),
                root: tmp.path().join("does-not-matter"),
            }],
        })
        .expect_err("should error");

        match err {
            AgentDetectError::UnknownConnectors { connectors } => {
                assert_eq!(connectors, vec!["definitely-unknown".to_string()]);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
