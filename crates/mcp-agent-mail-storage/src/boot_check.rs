//! Boot-time archive integrity preflight.
//!
//! The default path is read-only: it discovers archive git repositories,
//! checks whether they open cleanly, and reuses the git-2.51 recovery detector
//! for missing-ref findings. `AutoRepair` is intentionally narrower than the
//! detector: it writes a backup first, then prunes only refs already classified
//! by the recovery layer as safe-to-prune.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use chrono::{SecondsFormat, Utc};
use git2::Repository;
use mcp_agent_mail_core::git_lock::{RepoFlock, canonicalize_repo};
use mcp_agent_mail_core::{EvidenceLedgerEntry, append_evidence_entry_if_configured};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::recovery::{PrunableRef, RefCategory, detect_missing_refs};

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
            "auto_repair" => Some(Self::AutoRepair),
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

#[derive(Debug)]
enum CandidateCheck {
    Clean,
    Broken(BootCheckFinding),
    MissingRefs {
        refs: Vec<PrunableRef>,
        finding: BootCheckFinding,
    },
}

#[derive(Debug)]
struct AutoRepairOutcome {
    actions: Vec<String>,
    backup_path: Option<PathBuf>,
    before_refs: Vec<String>,
    after_refs: Vec<String>,
    pruned_refs: Vec<String>,
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
        repo_slug = %repo_slug,
        caller = CALLER,
        args_hash = %args_hash,
        duration_ms = 0_u64,
        outcome = "success",
        git_version = GIT_VERSION,
        mode = mode.observability_label(),
        root = %root.display(),
        total_projects,
        "boot_check_started"
    );

    let mut findings = Vec::new();
    let mut auto_repaired_count = 0_u32;
    for candidate in &candidates {
        match check_candidate(candidate) {
            CandidateCheck::Clean => {}
            CandidateCheck::Broken(finding) => findings.push(finding),
            CandidateCheck::MissingRefs { refs, finding } => {
                if mode != BootCheckMode::AutoRepair {
                    findings.push(finding);
                    continue;
                }

                match auto_repair_missing_refs(root, candidate, &refs) {
                    Ok(outcome) => {
                        emit_auto_repair_attempted(
                            &repo_slug, &args_hash, mode, candidate, &outcome,
                        );
                        record_auto_repair_evidence(candidate, &outcome);
                        if !outcome.pruned_refs.is_empty() {
                            auto_repaired_count = auto_repaired_count.saturating_add(1);
                        }
                        if !outcome.after_refs.is_empty() {
                            findings.push(missing_refs_finding_from_names(
                                candidate,
                                outcome.after_refs.clone(),
                            ));
                        }
                    }
                    Err(error) => {
                        emit_auto_repair_failed(&repo_slug, &args_hash, mode, candidate, &error);
                        findings.push(BootCheckFinding {
                            project: finding.project,
                            kind: finding.kind,
                            detail: format!("auto repair failed: {error}; {}", finding.detail),
                        });
                    }
                }
            }
        }
    }
    for finding in &findings {
        tracing::warn!(
            target: TARGET,
            repo_slug = %repo_slug,
            caller = CALLER,
            args_hash = %args_hash,
            duration_ms = 0_u64,
            outcome = "error",
            git_version = GIT_VERSION,
            mode = mode.observability_label(),
            project = %finding.project,
            kind = finding_kind_label(&finding.kind),
            detail = %finding.detail,
            "boot_check_finding"
        );
    }

    let completed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let duration_ms = u64::try_from(timer.elapsed().as_millis()).unwrap_or(u64::MAX);
    let should_abort = mode == BootCheckMode::Abort && !findings.is_empty();
    tracing::info!(
        target: TARGET,
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
        auto_repaired_count,
        degraded = !findings.is_empty(),
        "boot_check_completed"
    );
    if should_abort {
        tracing::error!(
            target: TARGET,
            repo_slug = %repo_slug,
            caller = CALLER,
            args_hash = %args_hash,
            duration_ms,
            outcome = "error",
            git_version = GIT_VERSION,
            mode = mode.observability_label(),
            total_projects,
            findings_count = findings.len(),
            auto_repaired_count,
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
        auto_repaired_count,
    }
}

fn finding_kind_label(kind: &BootCheckFindingKind) -> &'static str {
    match kind {
        BootCheckFindingKind::RepoBroken => "repo_broken",
        BootCheckFindingKind::OrphanRefs(_) => "orphan_refs",
        BootCheckFindingKind::DanglingBranch(_) => "dangling_branch",
        BootCheckFindingKind::ConfigCorrupt(_) => "config_corrupt",
        BootCheckFindingKind::AutoRepaired { .. } => "auto_repaired",
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

fn check_candidate(candidate: &ArchiveRepoCandidate) -> CandidateCheck {
    if let Err(error) = Repository::open(&candidate.path) {
        return CandidateCheck::Broken(BootCheckFinding {
            project: candidate.project.clone(),
            kind: BootCheckFindingKind::RepoBroken,
            detail: format!(
                "git repository open failed at {}: {error}",
                candidate.path.display()
            ),
        });
    }

    match detect_missing_refs(&candidate.path) {
        Ok(refs) if refs.is_empty() => CandidateCheck::Clean,
        Ok(refs) => {
            let finding = missing_refs_finding(candidate, &refs);
            CandidateCheck::MissingRefs { refs, finding }
        }
        Err(error) => CandidateCheck::Broken(BootCheckFinding {
            project: candidate.project.clone(),
            kind: BootCheckFindingKind::RepoBroken,
            detail: format!(
                "git ref integrity scan failed at {}: {error}",
                candidate.path.display()
            ),
        }),
    }
}

fn missing_refs_finding(
    candidate: &ArchiveRepoCandidate,
    refs: &[PrunableRef],
) -> BootCheckFinding {
    let ref_names = refs
        .iter()
        .map(|finding| finding.ref_name.clone())
        .collect::<Vec<_>>();
    missing_refs_finding_from_names(candidate, ref_names)
}

fn missing_refs_finding_from_names(
    candidate: &ArchiveRepoCandidate,
    ref_names: Vec<String>,
) -> BootCheckFinding {
    BootCheckFinding {
        project: candidate.project.clone(),
        detail: format!(
            "{} missing ref target(s): {}",
            ref_names.len(),
            ref_names.join(", ")
        ),
        kind: BootCheckFindingKind::OrphanRefs(ref_names),
    }
}

fn auto_repair_missing_refs(
    root: &Path,
    candidate: &ArchiveRepoCandidate,
    refs: &[PrunableRef],
) -> Result<AutoRepairOutcome, String> {
    let before_refs = refs
        .iter()
        .map(|finding| finding.ref_name.clone())
        .collect::<Vec<_>>();
    let safe_refs = refs
        .iter()
        .filter(|finding| finding.category == RefCategory::SafeToPrune)
        .collect::<Vec<_>>();
    let mut actions = Vec::new();
    if safe_refs.is_empty() {
        actions.push("refused_no_safe_refs".to_string());
        return Ok(AutoRepairOutcome {
            actions,
            backup_path: None,
            before_refs: before_refs.clone(),
            after_refs: before_refs,
            pruned_refs: Vec::new(),
        });
    }

    let canonical = canonicalize_repo(&candidate.path)
        .ok_or_else(|| format!("canonicalize repo {}", candidate.path.display()))?;
    let _flock = RepoFlock::acquire(&canonical)
        .map_err(|error| format!("acquire repo lock {}: {error}", canonical.display()))?;

    let backup_path = write_ref_backup(root, candidate, refs)?;
    actions.push(format!("backup_refs:{}", backup_path.display()));

    let mut pruned_refs = Vec::new();
    for finding in safe_refs {
        prune_ref(&candidate.path, &finding.ref_name)?;
        actions.push(format!("prune_ref:{}", finding.ref_name));
        pruned_refs.push(finding.ref_name.clone());
    }

    repack_refs(root, candidate)?;
    actions.push("repack_refs".to_string());

    let after = detect_missing_refs(&candidate.path)
        .map_err(|error| format!("post-repair missing-ref scan failed: {error}"))?;
    let after_refs = after
        .into_iter()
        .map(|finding| finding.ref_name)
        .collect::<Vec<_>>();

    Ok(AutoRepairOutcome {
        actions,
        backup_path: Some(backup_path),
        before_refs,
        after_refs,
        pruned_refs,
    })
}

fn write_ref_backup(
    root: &Path,
    candidate: &ArchiveRepoCandidate,
    refs: &[PrunableRef],
) -> Result<PathBuf, String> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros())
        .unwrap_or(0);
    let backup_dir = root
        .join("backups")
        .join("refs")
        .join(safe_backup_project_name(&candidate.project));
    fs::create_dir_all(&backup_dir)
        .map_err(|error| format!("create backup dir {}: {error}", backup_dir.display()))?;
    let backup_path = backup_dir.join(format!("{ts}.txt"));
    let mut text = String::new();
    text.push_str(&format!("# boot-check auto-repair backup {ts}\n"));
    text.push_str(&format!("# project: {}\n", candidate.project));
    text.push_str(&format!("# repo: {}\n", candidate.path.display()));
    text.push_str("# format: <status> <ref_name> <target_sha> <category> <reason>\n");
    if let Ok(repo) = Repository::open(&candidate.path) {
        text.push_str("#\n# ALL refs at backup time:\n");
        if let Ok(references) = repo.references() {
            for reference in references.flatten() {
                if let (Some(name), Some(target)) = (reference.name(), reference.target()) {
                    text.push_str(&format!("ref  {name}  {target}\n"));
                }
            }
        }
        text.push_str("#\n");
    }
    text.push_str("# ORPHAN findings:\n");
    for finding in refs {
        text.push_str(&format!(
            "orphan  {}  {}  {:?}  {}\n",
            finding.ref_name, finding.target_sha, finding.category, finding.reason,
        ));
    }
    fs::write(&backup_path, text.as_bytes())
        .map_err(|error| format!("write backup {}: {error}", backup_path.display()))?;
    Ok(backup_path)
}

fn prune_ref(repo_path: &Path, ref_name: &str) -> Result<(), String> {
    let repo =
        Repository::open(repo_path).map_err(|error| format!("open repo for pruning: {error}"))?;
    let mut reference = repo
        .find_reference(ref_name)
        .map_err(|error| format!("find reference {ref_name}: {error}"))?;
    reference
        .delete()
        .map_err(|error| format!("delete reference {ref_name}: {error}"))?;
    Ok(())
}

fn repack_refs(root: &Path, candidate: &ArchiveRepoCandidate) -> Result<(), String> {
    let admin_dir = mcp_agent_mail_core::git_lock::admin_dir_for(&candidate.path)
        .ok_or_else(|| format!("resolve git admin dir for {}", candidate.path.display()))?;
    let packed_refs = admin_dir.join("packed-refs");
    if packed_refs.is_file() {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_micros())
            .unwrap_or(0);
        let backup_dir = root
            .join("backups")
            .join("refs")
            .join(safe_backup_project_name(&candidate.project));
        fs::create_dir_all(&backup_dir)
            .map_err(|error| format!("create packed-refs backup dir: {error}"))?;
        let backup_path = backup_dir.join(format!("{ts}-packed-refs.txt"));
        fs::copy(&packed_refs, &backup_path).map_err(|error| {
            format!(
                "copy packed-refs backup {} -> {}: {error}",
                packed_refs.display(),
                backup_path.display()
            )
        })?;
    }

    let output = mcp_agent_mail_core::git_cmd::GitCmd::new(&candidate.path)
        .args(["pack-refs", "--all", "--prune"])
        .skip_flock()
        .run()
        .map_err(|error| format!("pack-refs invocation: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "pack-refs exit {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    Ok(())
}

fn safe_backup_project_name(project: &str) -> String {
    project
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn emit_auto_repair_attempted(
    repo_slug: &str,
    args_hash: &str,
    mode: BootCheckMode,
    candidate: &ArchiveRepoCandidate,
    outcome: &AutoRepairOutcome,
) {
    let before_state = json!({ "missing_refs": outcome.before_refs });
    let after_state = json!({ "missing_refs": outcome.after_refs });
    tracing::warn!(
        target: TARGET,
        repo_slug = %repo_slug,
        caller = CALLER,
        args_hash = %args_hash,
        duration_ms = 0_u64,
        outcome = "success",
        git_version = GIT_VERSION,
        mode = mode.observability_label(),
        project = %candidate.project,
        actions = ?outcome.actions,
        before_state = %before_state,
        after_state = %after_state,
        "boot_check_auto_repair_attempted"
    );
}

fn emit_auto_repair_failed(
    repo_slug: &str,
    args_hash: &str,
    mode: BootCheckMode,
    candidate: &ArchiveRepoCandidate,
    error: &str,
) {
    tracing::error!(
        target: TARGET,
        repo_slug = %repo_slug,
        caller = CALLER,
        args_hash = %args_hash,
        duration_ms = 0_u64,
        outcome = "error",
        git_version = GIT_VERSION,
        mode = mode.observability_label(),
        project = %candidate.project,
        actions = ?["backup_refs", "prune_safe_refs", "repack_refs"],
        error = %error,
        "boot_check_auto_repair_failed"
    );
}

fn record_auto_repair_evidence(candidate: &ArchiveRepoCandidate, outcome: &AutoRepairOutcome) {
    if outcome.pruned_refs.is_empty() {
        return;
    }
    let evidence = json!({
        "project": candidate.project,
        "repo": candidate.path,
        "actions": outcome.actions,
        "backup_path": outcome.backup_path,
        "before_state": { "missing_refs": outcome.before_refs },
        "after_state": { "missing_refs": outcome.after_refs },
        "pruned_refs": outcome.pruned_refs,
    });
    let entry = EvidenceLedgerEntry::new(
        format!("boot_check_auto_repair:{}", candidate.project),
        "boot_check.auto_repair",
        "prune_safe_missing_refs",
        1.0,
        evidence,
    );
    if let Err(error) = append_evidence_entry_if_configured(&entry) {
        tracing::warn!(
            target: TARGET,
            project = %candidate.project,
            err = %error,
            "boot_check_auto_repair_evidence_write_failed"
        );
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

    fn backup_files(root: &Path, project: &str) -> Vec<PathBuf> {
        let dir = root.join("backups").join("refs").join(project);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        entries.flatten().map(|entry| entry.path()).collect()
    }

    #[test]
    fn boot_check_mode_parse_accepts_documented_values() {
        assert_eq!(BootCheckMode::parse("warn"), Some(BootCheckMode::Warn));
        assert_eq!(BootCheckMode::parse("abort"), Some(BootCheckMode::Abort));
        assert_eq!(
            BootCheckMode::parse("auto_repair"),
            Some(BootCheckMode::AutoRepair)
        );
        assert_eq!(BootCheckMode::parse("off"), None);
        assert_eq!(BootCheckMode::parse("enforce"), None);
        assert_eq!(BootCheckMode::parse("auto-repair"), None);
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
    fn preflight_auto_repair_prunes_safe_orphan_refs_after_backup() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());
        let fake = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let stash_ref = tmp.path().join(".git/refs/stash");
        std::fs::write(&stash_ref, format!("{fake}\n")).unwrap();

        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::AutoRepair);

        assert_eq!(report.total_projects, 1);
        assert!(!report.has_findings());
        assert_eq!(report.auto_repaired_count, 1);
        assert!(!stash_ref.exists());

        let backups = backup_files(tmp.path(), ARCHIVE_ROOT_LABEL);
        assert!(
            backups.iter().any(|path| std::fs::read_to_string(path)
                .is_ok_and(|text| text.contains("orphan  refs/stash"))),
            "expected refs/stash backup in {backups:?}"
        );
    }

    #[test]
    fn preflight_auto_repair_refuses_non_safe_refs_without_pruning() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());
        let broken_ref = tmp.path().join(".git/refs/heads/broken");
        std::fs::write(&broken_ref, "cafebabecafebabecafebabecafebabecafebabe\n").unwrap();

        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::AutoRepair);

        assert_eq!(report.total_projects, 1);
        assert!(report.has_findings());
        assert_eq!(report.auto_repaired_count, 0);
        assert!(!report.should_abort());
        assert!(broken_ref.exists());
        assert!(backup_files(tmp.path(), ARCHIVE_ROOT_LABEL).is_empty());
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
