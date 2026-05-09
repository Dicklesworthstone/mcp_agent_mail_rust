//! Boot-time archive integrity preflight.
//!
//! This module is intentionally read-only for its first slice: it discovers
//! archive git repositories, checks whether they open cleanly, and reuses the
//! git-2.51 recovery detector for missing-ref findings.

use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{SecondsFormat, Utc};
use git2::Repository;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::recovery::detect_missing_refs;

const TARGET: &str = "mcp_agent_mail::boot_check";
const CALLER: &str = "startup.boot_check";
const GIT_VERSION: &str = "libgit2";
const ARCHIVE_ROOT_LABEL: &str = "archive-root";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BootCheckMode {
    Warn,
    Abort,
    AutoRepair,
}

impl BootCheckMode {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "warn" => Some(Self::Warn),
            "abort" => Some(Self::Abort),
            "auto_repair" | "auto-repair" => Some(Self::AutoRepair),
            _ => None,
        }
    }

    const fn observability_label(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Abort => "abort",
            Self::AutoRepair => "auto_repair",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BootCheckFinding {
    pub project: String,
    pub kind: BootCheckFindingKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BootCheckFindingKind {
    RepoBroken,
    OrphanRefs(Vec<String>),
    DanglingBranch(String),
    ConfigCorrupt(String),
    AutoRepaired { actions: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BootCheckReport {
    pub mode: BootCheckMode,
    pub root: PathBuf,
    pub started_at: String,
    pub completed_at: String,
    pub duration_ms: u64,
    pub total_projects: u32,
    pub findings: Vec<BootCheckFinding>,
    pub auto_repaired_count: u32,
}

impl BootCheckReport {
    #[must_use]
    pub fn has_findings(&self) -> bool {
        !self.findings.is_empty()
    }

    #[must_use]
    pub fn should_abort(&self) -> bool {
        self.mode == BootCheckMode::Abort && self.has_findings()
    }
}

#[derive(Debug)]
struct ArchiveRepoCandidate {
    project: String,
    path: PathBuf,
}

/// Read-only boot preflight for archive git repositories.
#[must_use]
pub fn preflight_archive_integrity(root: &Path, mode: BootCheckMode) -> BootCheckReport {
    let started = Utc::now();
    let started_at = started.to_rfc3339_opts(SecondsFormat::Micros, true);
    let timer = Instant::now();
    let repo_slug = mcp_agent_mail_core::slugify(&root.display().to_string());
    let args_hash = boot_check_args_hash(root, mode);
    let candidates = archive_repo_candidates(root);
    let total_projects = u32::try_from(candidates.len()).unwrap_or(u32::MAX);

    tracing::info!(
        target: TARGET,
        root = %root.display(),
        repo_slug = %repo_slug,
        caller = CALLER,
        args_hash = %args_hash,
        duration_ms = 0_u64,
        outcome = "success",
        git_version = GIT_VERSION,
        mode = mode.observability_label(),
        total_projects,
        "boot_check_started"
    );

    let mut findings = Vec::new();
    for candidate in &candidates {
        if let Some(finding) = check_candidate(candidate) {
            tracing::warn!(
                target: TARGET,
                root = %root.display(),
                repo_slug = %repo_slug,
                caller = CALLER,
                args_hash = %args_hash,
                duration_ms = 0_u64,
                outcome = "degraded",
                git_version = GIT_VERSION,
                mode = mode.observability_label(),
                project = %finding.project,
                kind = finding.kind.label(),
                detail = %finding.detail,
                "boot_check_finding"
            );
            findings.push(finding);
        }
    }

    let completed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let duration_ms = u64::try_from(timer.elapsed().as_millis()).unwrap_or(u64::MAX);
    let should_abort = mode == BootCheckMode::Abort && !findings.is_empty();

    tracing::info!(
        target: TARGET,
        root = %root.display(),
        repo_slug = %repo_slug,
        caller = CALLER,
        args_hash = %args_hash,
        duration_ms,
        outcome = if should_abort {
            "error"
        } else {
            "success"
        },
        git_version = GIT_VERSION,
        mode = mode.observability_label(),
        total_projects,
        findings_count = findings.len(),
        auto_repaired_count = 0_u32,
        degraded = !findings.is_empty(),
        "boot_check_completed"
    );

    if should_abort {
        tracing::error!(
            target: TARGET,
            root = %root.display(),
            repo_slug = %repo_slug,
            caller = CALLER,
            args_hash = %args_hash,
            duration_ms,
            outcome = "error",
            git_version = GIT_VERSION,
            mode = mode.observability_label(),
            total_projects,
            findings_count = findings.len(),
            "boot_check_aborted"
        );
    }

    BootCheckReport {
        mode,
        root: root.to_path_buf(),
        started_at,
        completed_at,
        duration_ms,
        total_projects,
        findings,
        auto_repaired_count: 0,
    }
}

impl BootCheckFindingKind {
    const fn label(&self) -> &'static str {
        match self {
            Self::RepoBroken => "repo_broken",
            Self::OrphanRefs(_) => "orphan_refs",
            Self::DanglingBranch(_) => "dangling_branch",
            Self::ConfigCorrupt(_) => "config_corrupt",
            Self::AutoRepaired { .. } => "auto_repaired",
        }
    }
}

fn boot_check_args_hash(root: &Path, mode: BootCheckMode) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update(mode.observability_label().as_bytes());
    hex::encode(hasher.finalize())
}

fn archive_repo_candidates(root: &Path) -> Vec<ArchiveRepoCandidate> {
    let mut candidates = Vec::new();
    if path_is_nonsymlink_dir(root) && has_git_metadata(root) {
        candidates.push(ArchiveRepoCandidate {
            project: ARCHIVE_ROOT_LABEL.to_string(),
            path: root.to_path_buf(),
        });
    }

    let projects = root.join("projects");
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return candidates;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_dir() || !has_git_metadata(&path) {
            continue;
        }
        let project = entry.file_name().to_string_lossy().into_owned();
        candidates.push(ArchiveRepoCandidate { project, path });
    }
    candidates
}

fn has_git_metadata(path: &Path) -> bool {
    let git = path.join(".git");
    if let Ok(meta) = std::fs::symlink_metadata(&git) {
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            return false;
        }
        if file_type.is_dir() || file_type.is_file() {
            return true;
        }
    }
    path_is_nonsymlink_file(&path.join("HEAD")) && path_is_nonsymlink_dir(&path.join("objects"))
}

fn path_is_nonsymlink_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir())
}

fn path_is_nonsymlink_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

fn check_candidate(candidate: &ArchiveRepoCandidate) -> Option<BootCheckFinding> {
    if let Err(error) = Repository::open(&candidate.path) {
        return Some(BootCheckFinding {
            project: candidate.project.clone(),
            kind: BootCheckFindingKind::RepoBroken,
            detail: format!(
                "git repository open failed at {}: {error}",
                candidate.path.display()
            ),
        });
    }

    match detect_missing_refs(&candidate.path) {
        Ok(refs) if refs.is_empty() => None,
        Ok(refs) => {
            let ref_names = refs
                .into_iter()
                .map(|finding| finding.ref_name)
                .collect::<Vec<_>>();
            Some(BootCheckFinding {
                project: candidate.project.clone(),
                detail: format!(
                    "{} missing ref target(s): {}",
                    ref_names.len(),
                    ref_names.join(", ")
                ),
                kind: BootCheckFindingKind::OrphanRefs(ref_names),
            })
        }
        Err(error) => Some(BootCheckFinding {
            project: candidate.project.clone(),
            kind: BootCheckFindingKind::RepoBroken,
            detail: format!(
                "git ref integrity scan failed at {}: {error}",
                candidate.path.display()
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;
    use tempfile::TempDir;

    fn init_repo_with_commit(dir: &Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "boot-check-test").unwrap();
        cfg.set_str("user.email", "boot-check@local").unwrap();

        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        {
            let tree = repo.find_tree(tree_oid).unwrap();
            let sig = Signature::now("boot", "boot@local").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();
        }
        repo
    }

    #[test]
    fn boot_check_mode_parse_accepts_documented_values() {
        assert_eq!(BootCheckMode::parse("warn"), Some(BootCheckMode::Warn));
        assert_eq!(BootCheckMode::parse("abort"), Some(BootCheckMode::Abort));
        assert_eq!(
            BootCheckMode::parse("auto_repair"),
            Some(BootCheckMode::AutoRepair)
        );
        assert_eq!(
            BootCheckMode::parse("auto-repair"),
            Some(BootCheckMode::AutoRepair)
        );
        assert_eq!(BootCheckMode::parse("other"), None);
    }

    #[test]
    fn boot_check_args_hash_is_stable_and_schema_shaped() {
        let first = boot_check_args_hash(Path::new("/data/projects/demo"), BootCheckMode::Warn);
        let second = boot_check_args_hash(Path::new("/data/projects/demo"), BootCheckMode::Warn);
        let different_mode =
            boot_check_args_hash(Path::new("/data/projects/demo"), BootCheckMode::Abort);

        assert_eq!(first, second);
        assert_ne!(first, different_mode);
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn preflight_missing_archive_root_has_no_findings() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("missing");
        let report = preflight_archive_integrity(&root, BootCheckMode::Warn);

        assert_eq!(report.total_projects, 0);
        assert!(report.findings.is_empty());
        assert!(!report.should_abort());
    }

    #[test]
    fn preflight_clean_archive_root_has_no_findings() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());

        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::Warn);

        assert_eq!(report.total_projects, 1);
        assert!(report.findings.is_empty());
        assert!(!report.has_findings());
    }

    #[test]
    fn preflight_detects_orphan_refs_in_archive_root() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());
        let fake = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        std::fs::write(tmp.path().join(".git/refs/stash"), format!("{fake}\n")).unwrap();

        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::Warn);

        assert_eq!(report.total_projects, 1);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].project, ARCHIVE_ROOT_LABEL);
        assert_eq!(
            report.findings[0].kind,
            BootCheckFindingKind::OrphanRefs(vec!["refs/stash".to_string()])
        );
    }

    #[test]
    fn preflight_abort_mode_reports_abort_when_findings_exist() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());
        std::fs::write(
            tmp.path().join(".git/refs/heads/broken"),
            "cafebabecafebabecafebabecafebabecafebabe\n",
        )
        .unwrap();

        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::Abort);

        assert!(report.has_findings());
        assert!(report.should_abort());
    }

    #[test]
    fn preflight_scans_project_repos_when_present() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path().join("projects");
        let project = projects.join("alpha");
        std::fs::create_dir_all(&project).unwrap();
        init_repo_with_commit(&project);
        std::fs::write(
            project.join(".git/refs/heads/broken"),
            "0000000000000000000000000000000000000001\n",
        )
        .unwrap();

        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::Warn);

        assert_eq!(report.total_projects, 1);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].project, "alpha");
    }

    #[cfg(unix)]
    #[test]
    fn preflight_skips_symlinked_project_repos() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        init_repo_with_commit(outside.path());
        std::fs::write(
            outside.path().join(".git/refs/heads/broken"),
            "0000000000000000000000000000000000000001\n",
        )
        .unwrap();

        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        symlink(outside.path(), projects.join("linked")).unwrap();

        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::Warn);

        assert_eq!(report.total_projects, 0);
        assert!(report.findings.is_empty());
    }

    #[test]
    fn preflight_reports_broken_repo_candidate() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git/config"), "not valid git config = [").unwrap();

        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::Warn);

        assert_eq!(report.total_projects, 1);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].kind, BootCheckFindingKind::RepoBroken);
    }
}
