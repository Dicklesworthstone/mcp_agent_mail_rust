//! Installed/active coding agent detection facade.
//!
//! With feature `agent-detect` enabled, this module re-exports detection primitives
//! from `franken-agent-detection` and supplements its filesystem probes with
//! Agent Mail's active OMP configuration resolver.
//! Without that feature, it preserves API shape and returns a deterministic
//! `FeatureDisabled` error.

#[cfg(feature = "agent-detect")]
pub use franken_agent_detection::{
    AgentDetectError, AgentDetectOptions, AgentDetectRootOverride, InstalledAgentDetectionEntry,
    InstalledAgentDetectionReport, InstalledAgentDetectionSummary,
};

/// Detect installed agents, including OMP's active runtime configuration root.
///
/// General probes and explicit root selection belong to `franken-agent-detection`.
/// OMP's runtime overrides share the resolver used by setup so automatic selection
/// reaches the same installation that setup will configure. Invalid active OMP
/// configuration is reported as undetected, without hiding other agents.
///
/// # Errors
///
/// Returns [`AgentDetectError::UnknownConnectors`] for unknown connector selectors.
#[cfg(feature = "agent-detect")]
pub fn detect_installed_agents(
    opts: &AgentDetectOptions,
) -> Result<InstalledAgentDetectionReport, AgentDetectError> {
    let mut probe_opts = opts.clone();
    probe_opts.include_undetected = true;
    let mut report = franken_agent_detection::detect_installed_agents(&probe_opts)?;

    // Explicit library roots retain their isolation from ambient runtime paths.
    let explicit_omp_root = opts.root_overrides.iter().any(|root| {
        crate::setup::AgentPlatform::from_slug(&root.slug.trim().to_ascii_lowercase())
            == Some(crate::setup::AgentPlatform::Omp)
    }) || std::env::var("CASS_OMP_DATA_ROOT")
        .is_ok_and(|root| !root.trim().is_empty());
    if !explicit_omp_root
        && let Some(entry) = report
            .installed_agents
            .iter_mut()
            .find(|entry| entry.slug == "omp")
    {
        match crate::setup::omp_config_paths_from_env() {
            Ok(Some(paths)) => {
                if let Some(root) = paths.user_mcp_config.parent().filter(|root| root.is_dir()) {
                    let root = root.to_string_lossy().into_owned();
                    entry.detected = true;
                    if !entry.root_paths.contains(&root) {
                        entry.root_paths.push(root.clone());
                    }
                    entry
                        .evidence
                        .push(format!("active OMP configuration directory exists: {root}"));
                }
            }
            Ok(None) => {}
            Err(error) => {
                entry.detected = false;
                entry.root_paths.clear();
                entry.evidence = vec![format!("active OMP configuration rejected: {error}")];
            }
        }
    }
    report.summary.detected_count = report
        .installed_agents
        .iter()
        .filter(|entry| entry.detected)
        .count();
    if !opts.include_undetected {
        report.installed_agents.retain(|entry| entry.detected);
    }
    Ok(report)
}

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

// These real-process fixtures isolate dirs::home_dir() through HOME. Windows
// resolves the profile through Known Folders instead and needs a native isolated
// user profile for this matrix; changing USERPROFILE would not isolate its reads.
#[cfg(all(test, feature = "agent-detect", unix))]
mod runtime_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    const CASE_ENV: &str = "AM_TEST_OMP_DETECTION_CASE";
    const ROOT_ENV: &str = "AM_TEST_OMP_DETECTION_ROOT";

    #[test]
    fn runtime_omp_overrides_are_detected() {
        run_detection_cases(
            "runtime_omp_overrides_are_detected",
            &["agent", "relative", "config", "omp_profile", "pi_profile"],
        );
    }

    #[test]
    fn runtime_omp_detection_preserves_controls() {
        run_detection_cases(
            "runtime_omp_detection_preserves_controls",
            &[
                "absent",
                "invalid",
                "roots",
                "cass",
                "cass_empty",
                "cass_whitespace",
                "filters",
            ],
        );
    }

    fn check_runtime_detection(case: &str) {
        let expected = std::env::var(ROOT_ENV).unwrap();
        let should_detect = !matches!(case, "absent" | "invalid" | "roots" | "cass");
        for include_undetected in [false, true] {
            // These are the same automatic options used by setup run/status.
            let mut opts = AgentDetectOptions {
                include_undetected,
                ..Default::default()
            };
            if case == "roots" {
                opts.root_overrides.push(AgentDetectRootOverride {
                    slug: " OH-MY-PI ".to_string(),
                    root: std::env::current_dir()
                        .unwrap()
                        .join("missing-explicit-root"),
                });
            }
            let report = detect_installed_agents(&opts).unwrap();
            let omp = report
                .installed_agents
                .iter()
                .find(|entry| entry.slug == "omp");
            assert_eq!(
                omp.is_some_and(|entry| entry.detected),
                should_detect,
                "{case}"
            );
            assert_eq!(omp.is_some(), include_undetected || should_detect, "{case}");
            if case == "invalid" {
                assert!(
                    report
                        .installed_agents
                        .iter()
                        .any(|entry| entry.slug == "codex" && entry.detected),
                    "an invalid OMP profile must not hide another installed agent"
                );
            }
            assert_eq!(
                report.summary.detected_count,
                report
                    .installed_agents
                    .iter()
                    .filter(|entry| entry.detected)
                    .count()
            );
            if should_detect {
                assert!(
                    omp.unwrap().root_paths.contains(&expected),
                    "{case}: {report:?}"
                );
            } else if case == "invalid" && include_undetected {
                let entry = omp.unwrap();
                assert_eq!(entry.root_paths, Vec::<String>::new());
                assert!(
                    entry
                        .evidence
                        .iter()
                        .any(|line| line.contains("invalid OMP profile"))
                );
            }
            if case == "filters" {
                opts.only_connectors = Some(vec!["codex".to_string()]);
                let filtered = detect_installed_agents(&opts).unwrap();
                assert_eq!(filtered.summary.total_count, 1);
                assert!(
                    filtered
                        .installed_agents
                        .iter()
                        .all(|entry| entry.slug == "codex")
                );
                opts.only_connectors = Some(vec![" OH-MY-PI ".to_string()]);
                let aliased = detect_installed_agents(&opts).unwrap();
                assert_eq!(aliased.summary.total_count, 1);
                assert_eq!(aliased.summary.detected_count, 1);
                opts.only_connectors = Some(vec!["not-a-connector".to_string()]);
                assert!(matches!(
                    detect_installed_agents(&opts),
                    Err(AgentDetectError::UnknownConnectors { .. })
                ));
            }
        }
    }

    fn configure_detection_child(
        command: &mut std::process::Command,
        case: &str,
        home: &Path,
        project: &Path,
    ) -> PathBuf {
        let custom = project.join("relocated-agent");
        match case {
            "config" => {
                command.env("PI_CONFIG_DIR", ".relocated-omp");
                home.join(".relocated-omp/agent")
            }
            "omp_profile" | "pi_profile" => {
                command.env("PI_CONFIG_DIR", ".relocated-omp");
                command.env(
                    if case == "omp_profile" {
                        "OMP_PROFILE"
                    } else {
                        "PI_PROFILE"
                    },
                    "work",
                );
                command.env("PI_CODING_AGENT_DIR", &custom);
                home.join(".relocated-omp/profiles/work/agent")
            }
            "invalid" => {
                command.env("OMP_PROFILE", "../escape");
                home.join(".omp/agent")
            }
            "relative" => {
                command.env("PI_CODING_AGENT_DIR", "relocated-agent");
                custom
            }
            _ => {
                if case != "absent" {
                    command.env("PI_CODING_AGENT_DIR", &custom);
                }
                if case == "cass" {
                    command.env("CASS_OMP_DATA_ROOT", project.join("missing-cass-root"));
                } else if case == "cass_empty" {
                    command.env("CASS_OMP_DATA_ROOT", "");
                } else if case == "cass_whitespace" {
                    command.env("CASS_OMP_DATA_ROOT", " \t ");
                }
                custom
            }
        }
    }

    fn run_detection_cases(test: &str, cases: &[&str]) {
        if let Ok(case) = std::env::var(CASE_ENV) {
            assert!(cases.contains(&case.as_str()));
            check_runtime_detection(&case);
            println!("{CASE_ENV}:{case}:executed");
            return;
        }
        for case in cases {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let home = root.join("home");
            let project = root.join("project");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&project).unwrap();
            if *case == "invalid" {
                std::fs::create_dir_all(home.join(".codex/sessions")).unwrap();
            }
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    &format!("agent_detect::runtime_tests::{test}"),
                    "--nocapture",
                ])
                .current_dir(&project)
                .env(CASE_ENV, case)
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("XDG_DATA_HOME", home.join(".local/share"));
            for key in [
                "OMP_PROFILE",
                "PI_PROFILE",
                "PI_CONFIG_DIR",
                "PI_CODING_AGENT_DIR",
                "CASS_OMP_DATA_ROOT",
                "CODEX_HOME",
            ] {
                command.env_remove(key);
            }
            let active = configure_detection_child(&mut command, case, &home, &project);
            if *case != "absent" {
                std::fs::create_dir_all(&active).unwrap();
                std::fs::write(active.join("mcp.json"), "{}\n").unwrap();
            }
            let output = command.env(ROOT_ENV, &active).output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{case}: {stdout}\n{stderr}");
            assert!(
                stdout.contains(&format!("{CASE_ENV}:{case}:executed")),
                "{stdout}\n{stderr}"
            );
        }
    }
}
