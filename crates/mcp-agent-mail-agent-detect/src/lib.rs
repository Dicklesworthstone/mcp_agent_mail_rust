//! Local coding-agent installation detection.
//!
//! Provides synchronous, filesystem-based probes for known coding-agent CLIs.
//! Intended for reuse by `mcp-agent-mail-core` and external tooling.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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

const KNOWN_CONNECTORS: &[&str] = &[
    "claude",
    "cline",
    "codex",
    "cursor",
    "factory",
    "gemini",
    "github-copilot",
    "opencode",
    "windsurf",
];

fn canonical_connector_slug(slug: &str) -> Option<&'static str> {
    match slug {
        "claude" | "claude-code" => Some("claude"),
        "cline" => Some("cline"),
        "codex" | "codex-cli" => Some("codex"),
        "cursor" => Some("cursor"),
        "factory" | "factory-droid" => Some("factory"),
        "gemini" | "gemini-cli" => Some("gemini"),
        "github-copilot" | "copilot" => Some("github-copilot"),
        "opencode" | "open-code" => Some("opencode"),
        "windsurf" => Some("windsurf"),
        _ => None,
    }
}

fn normalize_slug(raw: &str) -> Option<String> {
    let slug = raw.trim().to_ascii_lowercase();
    if slug.is_empty() { None } else { Some(slug) }
}

fn canonical_or_normalized_slug(raw: &str) -> Option<String> {
    let normalized = normalize_slug(raw)?;
    Some(canonical_connector_slug(&normalized).map_or(normalized, std::string::ToString::to_string))
}

fn home_join(parts: &[&str]) -> Option<PathBuf> {
    let mut path = dirs::home_dir()?;
    for part in parts {
        path.push(part);
    }
    Some(path)
}

fn default_probe_roots(slug: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push = |parts: &[&str]| {
        if let Some(path) = home_join(parts) {
            out.push(path);
        }
    };

    match slug {
        "claude" => {
            push(&[".claude"]);
            push(&[".config", "claude"]);
        }
        "cline" => {
            push(&[".cline"]);
            push(&[".config", "cline"]);
        }
        "codex" => {
            push(&[".codex"]);
            push(&[".config", "codex"]);
        }
        "cursor" => {
            push(&[".cursor"]);
            push(&[".config", "Cursor"]);
        }
        "factory" => {
            push(&[".factory-droid"]);
            push(&[".config", "factory-droid"]);
        }
        "gemini" => {
            push(&[".gemini"]);
            push(&[".config", "gemini"]);
        }
        "github-copilot" => {
            push(&[".github-copilot"]);
            push(&[".config", "github-copilot"]);
        }
        "opencode" => {
            push(&[".opencode"]);
            push(&[".config", "opencode"]);
        }
        "windsurf" => {
            push(&[".windsurf"]);
            push(&[".config", "windsurf"]);
        }
        _ => {}
    }

    out
}

fn detect_roots(
    slug: &'static str,
    roots: &[PathBuf],
    source_label: &str,
) -> InstalledAgentDetectionEntry {
    let mut detected = false;
    let mut evidence: Vec<String> = Vec::new();
    let mut root_paths: Vec<String> = Vec::new();

    if roots.is_empty() {
        evidence.push("no probe roots available".to_string());
    }

    for root in roots {
        let root_str = root.display().to_string();
        if root.exists() {
            detected = true;
            root_paths.push(root_str.clone());
            evidence.push(format!("{source_label} root exists: {root_str}"));
        } else {
            evidence.push(format!("{source_label} root missing: {root_str}"));
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

fn entry_from_detect(slug: &'static str) -> InstalledAgentDetectionEntry {
    let roots = default_probe_roots(slug);
    detect_roots(slug, &roots, "default")
}

fn entry_from_override(slug: &'static str, roots: &[PathBuf]) -> InstalledAgentDetectionEntry {
    detect_roots(slug, roots, "override")
}

fn build_overrides_map(overrides: &[AgentDetectRootOverride]) -> HashMap<String, Vec<PathBuf>> {
    let mut out: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for override_root in overrides {
        let Some(slug) = canonical_or_normalized_slug(&override_root.slug) else {
            continue;
        };
        out.entry(slug)
            .or_default()
            .push(override_root.root.clone());
    }
    out
}

fn validate_known_connectors(
    available: &HashSet<&'static str>,
    only: Option<&HashSet<String>>,
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

/// Detect installed/available coding agents by running local filesystem probes.
///
/// This returns a stable JSON shape (via `serde`) intended for CLI/resource consumption.
///
/// # Errors
/// Returns [`AgentDetectError::UnknownConnectors`] when `only_connectors`
/// includes unknown slugs.
#[allow(clippy::missing_const_for_fn)]
pub fn detect_installed_agents(
    opts: &AgentDetectOptions,
) -> Result<InstalledAgentDetectionReport, AgentDetectError> {
    let available: HashSet<&'static str> = KNOWN_CONNECTORS.iter().copied().collect();
    let overrides = build_overrides_map(&opts.root_overrides);

    let only: Option<HashSet<String>> = opts.only_connectors.as_ref().map(|slugs| {
        slugs
            .iter()
            .filter_map(|slug| canonical_or_normalized_slug(slug))
            .collect()
    });

    validate_known_connectors(&available, only.as_ref(), &overrides)?;

    let mut all_entries: Vec<InstalledAgentDetectionEntry> = KNOWN_CONNECTORS
        .iter()
        .copied()
        .filter(|slug| only.as_ref().is_none_or(|set| set.contains(*slug)))
        .map(|slug| {
            overrides.get(slug).map_or_else(
                || entry_from_detect(slug),
                |roots| entry_from_override(slug, roots),
            )
        })
        .collect();

    all_entries.sort_by(|a, b| a.slug.cmp(&b.slug));

    let detected_count = all_entries.iter().filter(|entry| entry.detected).count();
    let total_count = all_entries.len();

    Ok(InstalledAgentDetectionReport {
        format_version: 1,
        generated_at: chrono::Utc::now().to_rfc3339(),
        installed_agents: if opts.include_undetected {
            all_entries
        } else {
            all_entries
                .into_iter()
                .filter(|entry| entry.detected)
                .collect()
        },
        summary: InstalledAgentDetectionSummary {
            detected_count,
            total_count,
        },
    })
}

#[cfg(test)]
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
                    root: codex_root,
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
            .map(|entry| entry.slug.as_str())
            .collect();
        assert_eq!(slugs, vec!["codex", "gemini"]);

        let codex = report
            .installed_agents
            .iter()
            .find(|entry| entry.slug == "codex")
            .expect("codex entry");
        assert!(codex.detected);
        assert!(
            codex
                .root_paths
                .iter()
                .any(|path| path.ends_with("/sessions"))
        );

        let gemini = report
            .installed_agents
            .iter()
            .find(|entry| entry.slug == "gemini")
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
            AgentDetectError::FeatureDisabled => {
                panic!("unexpected error: FeatureDisabled")
            }
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
            AgentDetectError::FeatureDisabled => {
                panic!("unexpected error: FeatureDisabled")
            }
        }
    }
}
