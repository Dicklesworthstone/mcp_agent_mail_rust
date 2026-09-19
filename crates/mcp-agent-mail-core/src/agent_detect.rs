//! Installed/active coding agent detection facade.
//!
//! With feature `agent-detect` enabled, this module re-exports detection primitives
//! from `franken-agent-detection` and supplements its filesystem probes with
//! Agent Mail's active OMP configuration resolver, fresh remote-client config
//! roots, and Unix PATH probes. Discovery never executes a candidate binary.
//! Without that feature, it preserves API shape and returns a deterministic
//! `FeatureDisabled` error.

#[cfg(feature = "agent-detect")]
pub use franken_agent_detection::{
    AgentDetectError, AgentDetectOptions, AgentDetectRootOverride, InstalledAgentDetectionEntry,
    InstalledAgentDetectionReport, InstalledAgentDetectionSummary,
};

/// Detect installed agents without requiring previous conversation history.
///
/// General probes and explicit root selection belong to `franken-agent-detection`.
/// The installer also admits fresh Codex/OMP config directories and executables
/// on PATH. Native setup must select those clients too, rather than require a
/// sessions directory before it will persist their bearer credential.
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

    supplement_remote_client_config_roots(&mut report, opts);
    #[cfg(unix)]
    supplement_remote_client_executables(&mut report, opts);

    // Explicit library roots retain their isolation from ambient runtime paths.
    let explicit_omp_root =
        explicit_root_selected(opts, crate::setup::AgentPlatform::Omp, "CASS_OMP_DATA_ROOT");
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
                // A PATH hit is installation evidence, not permission to bypass
                // the active profile's authority validation.
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

#[cfg(feature = "agent-detect")]
fn supplement_remote_client_config_roots(
    report: &mut InstalledAgentDetectionReport,
    opts: &AgentDetectOptions,
) {
    use crate::setup::AgentPlatform;

    let Some(home) = dirs::home_dir().filter(|home| home.is_absolute()) else {
        return;
    };
    // These are installation witnesses shared with install.sh, not new write
    // destinations. In particular .config/codex is evidence for Codex while
    // setup still owns the choice and protection of its canonical config file.
    // Keep explicit library/runtime roots isolated from the default HOME.
    for (platform, env_key, roots) in [
        (
            AgentPlatform::Codex,
            "CODEX_HOME",
            &[".codex", ".config/codex"][..],
        ),
        (AgentPlatform::Omp, "CASS_OMP_DATA_ROOT", &[".omp"][..]),
    ] {
        if explicit_root_selected(opts, platform, env_key) {
            continue;
        }
        let Some(entry) = report
            .installed_agents
            .iter_mut()
            .find(|entry| entry.slug == platform.slug())
        else {
            continue;
        };
        for root in roots.iter().map(|relative| home.join(relative)) {
            // A file or dangling link named .codex/.omp is not an install.
            // A real-directory symlink is presence evidence only: it never
            // bypasses the setup writer's independent no-follow admission.
            if root.is_dir() {
                entry.detected = true;
                let root = root.to_string_lossy().into_owned();
                if !entry.root_paths.contains(&root) {
                    entry.root_paths.push(root.clone());
                    entry.evidence.push(format!(
                        "remote MCP client configuration directory exists: {root}"
                    ));
                }
            }
        }
    }
}

#[cfg(feature = "agent-detect")]
fn explicit_root_selected(
    opts: &AgentDetectOptions,
    platform: crate::setup::AgentPlatform,
    env_key: &str,
) -> bool {
    opts.root_overrides.iter().any(|root| {
        crate::setup::AgentPlatform::from_slug(&root.slug.trim().to_ascii_lowercase())
            == Some(platform)
    }) || std::env::var_os(env_key).is_some_and(|root| !root.to_string_lossy().trim().is_empty())
}

#[cfg(all(feature = "agent-detect", unix))]
fn supplement_remote_client_executables(
    report: &mut InstalledAgentDetectionReport,
    opts: &AgentDetectOptions,
) {
    use crate::setup::AgentPlatform;
    use nix::unistd::{AccessFlags, access};

    let Some(path) = std::env::var_os("PATH") else {
        return;
    };
    // Match install.sh's remote_http_client_target_tools, not arbitrary commands
    // or shell aliases. Explicit probe roots must not inherit ambient agents.
    for (platform, env_key) in [
        (AgentPlatform::Codex, "CODEX_HOME"),
        (AgentPlatform::Omp, "CASS_OMP_DATA_ROOT"),
    ] {
        if explicit_root_selected(opts, platform, env_key) {
            continue;
        }
        let Some(entry) = report
            .installed_agents
            .iter_mut()
            .find(|entry| entry.slug == platform.slug() && !entry.detected)
        else {
            continue;
        };
        // split_paths preserves non-UTF-8 names and the shell's explicit empty
        // and relative PATH components. A directory with search permission is
        // not an agent. Follow package-manager symlinks, but never run --version
        // (or any other candidate code) just to establish installation presence.
        if let Some(binary) = std::env::split_paths(&path)
            .map(|directory| directory.join(platform.slug()))
            .find(|candidate| {
                candidate.is_file() && access(candidate.as_path(), AccessFlags::X_OK).is_ok()
            })
        {
            entry.detected = true;
            entry.evidence.push(format!(
                "remote MCP client executable on PATH: {}",
                binary.display()
            ));
            // root_paths describe config/data roots, never executable locations.
            // Setup retains responsibility for choosing and securing its writes.
        }
    }
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
    const PATH_CASE_ENV: &str = "AM_TEST_PATH_DETECTION_CASE";

    #[test]
    fn path_only_remote_clients_preserve_discovery_authority() {
        run_remote_client_cases(
            "path_only_remote_clients_preserve_discovery_authority",
            &[
                "executable",
                "symlink",
                "relative",
                "empty_component",
                "non_utf8",
                "shadowed",
                "absent",
                "non_executable",
                "directory",
                "dangling",
                "unset_path",
                "roots",
                "env_roots",
                "env_empty",
                "env_whitespace",
                "invalid_omp",
            ],
        );
    }

    #[test]
    fn configuration_only_remote_clients_preserve_discovery_authority() {
        run_remote_client_cases(
            "configuration_only_remote_clients_preserve_discovery_authority",
            &[
                "config_directory",
                "config_xdg",
                "config_roots",
                "config_env_roots",
                "config_file",
                "config_dangling",
                "config_invalid_omp",
            ],
        );
    }

    fn run_remote_client_cases(test: &str, cases: &[&str]) {
        if let Ok(case) = std::env::var(PATH_CASE_ENV) {
            assert!(cases.contains(&case.as_str()));
            check_path_detection(&case);
            println!("{PATH_CASE_ENV}:{case}:executed");
            return;
        }
        for &case in cases {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let home = root.join("home");
            let project = root.join("project");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&project).unwrap();
            let marker = root.join("candidate-executed");
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    &format!("agent_detect::runtime_tests::{test}"),
                    "--nocapture",
                ])
                .current_dir(&project)
                .env(PATH_CASE_ENV, case)
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .env("AM_TEST_EXECUTED_BINARY_MARKER", &marker);
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
            configure_path_child(&mut command, case, &home, &project);
            let before = detection_tree_entries(&home);
            let output = command.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{case}: {stdout}\n{stderr}");
            assert!(
                stdout.contains(&format!("{PATH_CASE_ENV}:{case}:executed")),
                "{case}: child did not execute its assertions: {stdout}\n{stderr}"
            );
            assert!(
                !marker.exists(),
                "{case}: discovery executed an agent binary"
            );
            assert_eq!(
                detection_tree_entries(&home),
                before,
                "{case}: discovery wrote files"
            );
        }
    }

    fn detection_tree_entries(root: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                paths.extend(detection_tree_entries(&path));
            }
            paths.push(path);
        }
        paths.sort();
        paths
    }

    fn check_path_detection(case: &str) {
        let available = !matches!(
            case,
            "absent"
                | "non_executable"
                | "directory"
                | "dangling"
                | "unset_path"
                | "roots"
                | "env_roots"
                | "config_roots"
                | "config_env_roots"
                | "config_file"
                | "config_dangling"
        );
        for include_undetected in [false, true] {
            for selectors in [
                vec!["codex", "omp"],
                vec![" CODEX-CLI "],
                vec![" OH-MY-PI "],
            ] {
                let mut opts = AgentDetectOptions {
                    only_connectors: Some(
                        selectors.iter().map(|slug| (*slug).to_string()).collect(),
                    ),
                    include_undetected,
                    ..Default::default()
                };
                if matches!(case, "roots" | "config_roots") {
                    for slug in [" CODEX-CLI ", " OH-MY-PI "] {
                        opts.root_overrides.push(AgentDetectRootOverride {
                            slug: slug.to_string(),
                            root: std::env::current_dir().unwrap().join("missing-probe-root"),
                        });
                    }
                }
                let report = detect_installed_agents(&opts).unwrap();
                assert_eq!(report.summary.total_count, selectors.len(), "{case}");
                let mut expected_count = 0;
                for selector in &selectors {
                    let platform = crate::setup::AgentPlatform::from_slug(
                        &selector.trim().to_ascii_lowercase(),
                    )
                    .unwrap();
                    let invalid_omp = matches!(case, "invalid_omp" | "config_invalid_omp")
                        && platform == crate::setup::AgentPlatform::Omp;
                    let expected = available && !invalid_omp;
                    expected_count += usize::from(expected);
                    let entry = report
                        .installed_agents
                        .iter()
                        .find(|entry| entry.slug == platform.slug());
                    assert_eq!(entry.is_some(), include_undetected || expected, "{case}");
                    assert_eq!(
                        entry.is_some_and(|entry| entry.detected),
                        expected,
                        "{case}"
                    );
                    if let Some(entry) = entry {
                        assert_remote_entry(entry, case, platform, expected);
                        if invalid_omp {
                            assert!(
                                entry
                                    .evidence
                                    .iter()
                                    .any(|line| line.contains("invalid OMP profile"))
                            );
                        }
                    }
                }
                assert_eq!(report.summary.detected_count, expected_count, "{case}");
                assert_eq!(
                    report.installed_agents.len(),
                    if include_undetected {
                        selectors.len()
                    } else {
                        expected_count
                    },
                    "{case}"
                );
            }
        }
        assert!(matches!(
            detect_installed_agents(&AgentDetectOptions {
                only_connectors: Some(vec!["not-a-connector".to_string()]),
                ..Default::default()
            }),
            Err(AgentDetectError::UnknownConnectors { .. })
        ));
    }

    fn assert_remote_entry(
        entry: &InstalledAgentDetectionEntry,
        case: &str,
        platform: crate::setup::AgentPlatform,
        expected: bool,
    ) {
        let config_only = case.starts_with("config_") && expected;
        if config_only {
            let relative = match platform {
                crate::setup::AgentPlatform::Codex if case == "config_xdg" => ".config/codex",
                crate::setup::AgentPlatform::Codex => ".codex",
                _ => ".omp",
            };
            let root = dirs::home_dir()
                .unwrap()
                .join(relative)
                .to_string_lossy()
                .into_owned();
            assert_eq!(entry.root_paths, vec![root], "{case}: {entry:?}");
        } else {
            assert!(entry.root_paths.is_empty(), "{case}: {entry:?}");
        }
        assert_eq!(
            entry
                .evidence
                .iter()
                .any(|line| line.contains("remote MCP client executable on PATH:")),
            expected && !config_only,
            "{case}: {entry:?}"
        );
    }

    fn write_path_candidate(case: &str, project: &Path, bin: &Path, slug: &str) {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let candidate = bin.join(slug);
        match case {
            case if case.starts_with("config_") => {}
            "absent" => {}
            "directory" => std::fs::create_dir(&candidate).unwrap(),
            "dangling" => symlink(project.join("missing-executable"), &candidate).unwrap(),
            _ => {
                let target = if case == "symlink" {
                    project.join(format!("{slug}-real"))
                } else {
                    candidate.clone()
                };
                std::fs::write(
                    &target,
                    "#!/bin/sh\nprintf executed > \"$AM_TEST_EXECUTED_BINARY_MARKER\"\nexit 91\n",
                )
                .unwrap();
                let mode = if case == "non_executable" {
                    0o644
                } else {
                    0o755
                };
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();
                if case == "symlink" {
                    symlink(&target, &candidate).unwrap();
                }
            }
        }
    }

    fn configure_path_child(
        command: &mut std::process::Command,
        case: &str,
        home: &Path,
        project: &Path,
    ) {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::PermissionsExt;

        let bin = if case == "non_utf8" {
            project.join(std::ffi::OsString::from_vec(b"bin-\xff".to_vec()))
        } else if case == "empty_component" {
            project.to_path_buf()
        } else {
            project.join("bin")
        };
        std::fs::create_dir_all(&bin).unwrap();
        for slug in ["codex", "omp"] {
            write_path_candidate(case, project, &bin, slug);
        }
        if case.starts_with("config_") {
            let codex_root = if case == "config_xdg" {
                ".config/codex"
            } else {
                ".codex"
            };
            for relative in [codex_root, ".omp"] {
                let root = home.join(relative);
                match case {
                    "config_file" => std::fs::write(&root, "not a config directory\n").unwrap(),
                    "config_dangling" => {
                        std::os::unix::fs::symlink(home.join("missing-root"), &root).unwrap();
                    }
                    _ => std::fs::create_dir_all(root).unwrap(),
                }
            }
        }
        command.env("PATH", &bin);
        match case {
            "relative" => {
                command.env("PATH", "bin");
            }
            "empty_component" => {
                command.env("PATH", ":");
            }
            "unset_path" => {
                command.env_remove("PATH");
            }
            "shadowed" => {
                let first = project.join("not-executable");
                std::fs::create_dir(&first).unwrap();
                for slug in ["codex", "omp"] {
                    let candidate = first.join(slug);
                    std::fs::write(&candidate, "not executable\n").unwrap();
                    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o644))
                        .unwrap();
                }
                command.env("PATH", std::env::join_paths([first, bin]).unwrap());
            }
            "env_roots" | "config_env_roots" => {
                for key in ["CODEX_HOME", "CASS_OMP_DATA_ROOT"] {
                    command.env(key, project.join("missing-probe-root"));
                }
            }
            "env_empty" | "env_whitespace" => {
                for key in ["CODEX_HOME", "CASS_OMP_DATA_ROOT"] {
                    command.env(key, if case == "env_empty" { "" } else { " \t " });
                }
            }
            "invalid_omp" | "config_invalid_omp" => {
                command.env("OMP_PROFILE", "../escape");
            }
            _ => {}
        }
    }

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
                .env("PATH", root.join("empty-path"))
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
