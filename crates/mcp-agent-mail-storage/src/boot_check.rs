//! Boot-time archive integrity preflight.
//!
//! The default path is read-only: it discovers archive git repositories,
//! checks whether they open cleanly, and reuses the git-2.51 recovery detector
//! for missing-ref findings. `AutoRepair` is intentionally narrower than the
//! detector: it writes a backup first, then prunes only refs already classified
//! by the recovery layer as safe-to-prune. Every deletion revalidates the
//! original target under the Git ref lock; a stale scan cannot delete a repair.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{SecondsFormat, Utc};
use git2::Repository;
use mcp_agent_mail_core::git_lock::{RepoFlock, canonicalize_repo};
use mcp_agent_mail_core::{EvidenceLedgerEntry, append_evidence_entry_if_configured};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::recovery::{
    PrunableRef, PruneRefOutcome, RefCategory, detect_missing_refs, prune_missing_ref, ref_backup,
};

const TARGET: &str = "mcp_agent_mail::boot_check";
const CALLER: &str = "startup.boot_check";
const GIT_VERSION: &str = "libgit2";
const ARCHIVE_ROOT_LABEL: &str = "archive-root";
const BOOT_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

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
    TimeoutExceeded { elapsed_ms: u64, timeout_ms: u64 },
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
    pub const fn has_findings(&self) -> bool {
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
        findings: Vec<BootCheckFinding>,
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
    preflight_archive_integrity_with_timeout(root, mode, BOOT_CHECK_TIMEOUT)
}

fn preflight_archive_integrity_with_timeout(
    root: &Path,
    mode: BootCheckMode,
    timeout: Duration,
) -> BootCheckReport {
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
    let mut effective_mode = mode;
    for (index, candidate) in candidates.iter().enumerate() {
        if let Some(timeout_finding) = timeout_finding_if_exceeded(root, timer.elapsed(), timeout) {
            effective_mode = BootCheckMode::Abort;
            emit_timeout_exceeded(
                &repo_slug,
                &args_hash,
                timeout_finding_elapsed_ms(&timeout_finding),
                duration_ms(timeout),
                total_projects,
                u32::try_from(index).unwrap_or(u32::MAX),
            );
            findings.push(timeout_finding);
            break;
        }

        match check_candidate(candidate) {
            CandidateCheck::Clean => {}
            CandidateCheck::Broken(finding) => findings.push(finding),
            CandidateCheck::MissingRefs {
                refs,
                findings: missing_ref_findings,
            } => {
                if mode != BootCheckMode::AutoRepair {
                    findings.extend(missing_ref_findings);
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
                            findings.extend(missing_refs_findings_from_names(
                                candidate,
                                &outcome.after_refs,
                            ));
                        }
                    }
                    Err(error) => {
                        emit_auto_repair_failed(&repo_slug, &args_hash, mode, candidate, &error);
                        findings.extend(missing_ref_findings.into_iter().map(|finding| {
                            BootCheckFinding {
                                project: finding.project,
                                kind: finding.kind,
                                detail: format!("auto repair failed: {error}; {}", finding.detail),
                            }
                        }));
                    }
                }
            }
        }

        if let Some(timeout_finding) = timeout_finding_if_exceeded(root, timer.elapsed(), timeout) {
            effective_mode = BootCheckMode::Abort;
            emit_timeout_exceeded(
                &repo_slug,
                &args_hash,
                timeout_finding_elapsed_ms(&timeout_finding),
                duration_ms(timeout),
                total_projects,
                u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX),
            );
            findings.push(timeout_finding);
            break;
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
            mode = effective_mode.observability_label(),
            project = %finding.project,
            kind = finding_kind_label(&finding.kind),
            detail = %finding.detail,
            "boot_check_finding"
        );
    }

    let completed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let duration_ms = u64::try_from(timer.elapsed().as_millis()).unwrap_or(u64::MAX);
    let should_abort = effective_mode == BootCheckMode::Abort && !findings.is_empty();
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
        mode = effective_mode.observability_label(),
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
            mode = effective_mode.observability_label(),
            total_projects,
            findings_count = findings.len(),
            auto_repaired_count,
            "boot_check_aborted"
        );
    }

    BootCheckReport {
        mode: effective_mode,
        root: root.to_path_buf(),
        started_at,
        completed_at,
        duration_ms,
        total_projects,
        findings,
        auto_repaired_count,
    }
}

const fn finding_kind_label(kind: &BootCheckFindingKind) -> &'static str {
    match kind {
        BootCheckFindingKind::RepoBroken => "repo_broken",
        BootCheckFindingKind::OrphanRefs(_) => "orphan_refs",
        BootCheckFindingKind::DanglingBranch(_) => "dangling_branch",
        BootCheckFindingKind::ConfigCorrupt(_) => "config_corrupt",
        BootCheckFindingKind::TimeoutExceeded { .. } => "timeout_exceeded",
        BootCheckFindingKind::AutoRepaired { .. } => "auto_repaired",
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn timeout_finding_if_exceeded(
    root: &Path,
    elapsed: Duration,
    timeout: Duration,
) -> Option<BootCheckFinding> {
    if elapsed < timeout {
        return None;
    }
    let elapsed_ms = duration_ms(elapsed);
    let timeout_ms = duration_ms(timeout);
    Some(BootCheckFinding {
        project: ARCHIVE_ROOT_LABEL.to_string(),
        kind: BootCheckFindingKind::TimeoutExceeded {
            elapsed_ms,
            timeout_ms,
        },
        detail: format!(
            "archive boot check exceeded timeout after {elapsed_ms}ms \
             while scanning {}",
            root.display()
        ),
    })
}

const fn timeout_finding_elapsed_ms(finding: &BootCheckFinding) -> u64 {
    match &finding.kind {
        BootCheckFindingKind::TimeoutExceeded { elapsed_ms, .. } => *elapsed_ms,
        _ => 0,
    }
}

fn emit_timeout_exceeded(
    repo_slug: &str,
    args_hash: &str,
    elapsed_ms: u64,
    timeout_ms: u64,
    total_projects: u32,
    scanned_projects: u32,
) {
    tracing::error!(
        target: TARGET,
        repo_slug = %repo_slug,
        caller = CALLER,
        args_hash = %args_hash,
        duration_ms = elapsed_ms,
        outcome = "error",
        git_version = GIT_VERSION,
        mode = BootCheckMode::Abort.observability_label(),
        total_projects,
        scanned_projects,
        timeout_ms,
        "boot_check_timeout_exceeded"
    );
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
            let findings = missing_refs_findings(candidate, &refs);
            CandidateCheck::MissingRefs { refs, findings }
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

fn missing_refs_findings(
    candidate: &ArchiveRepoCandidate,
    refs: &[PrunableRef],
) -> Vec<BootCheckFinding> {
    let mut findings = Vec::new();
    let mut orphan_ref_names = Vec::new();
    for missing_ref in refs {
        if missing_ref.ref_name.starts_with("refs/heads/") {
            findings.push(BootCheckFinding {
                project: candidate.project.clone(),
                detail: format!(
                    "dangling branch ref target missing: {} -> {} ({})",
                    missing_ref.ref_name, missing_ref.target_sha, missing_ref.reason
                ),
                kind: BootCheckFindingKind::DanglingBranch(missing_ref.ref_name.clone()),
            });
        } else {
            orphan_ref_names.push(missing_ref.ref_name.clone());
        }
    }
    findings.extend(missing_refs_findings_from_names(
        candidate,
        &orphan_ref_names,
    ));
    findings
}

fn missing_refs_findings_from_names(
    candidate: &ArchiveRepoCandidate,
    ref_names: &[String],
) -> Vec<BootCheckFinding> {
    let mut findings = Vec::new();
    let mut orphan_ref_names = Vec::new();
    for ref_name in ref_names {
        if ref_name.starts_with("refs/heads/") {
            findings.push(BootCheckFinding {
                project: candidate.project.clone(),
                detail: format!("dangling branch ref target still missing: {ref_name}"),
                kind: BootCheckFindingKind::DanglingBranch(ref_name.clone()),
            });
        } else {
            orphan_ref_names.push(ref_name.clone());
        }
    }
    if !orphan_ref_names.is_empty() {
        findings.push(BootCheckFinding {
            project: candidate.project.clone(),
            detail: format!(
                "{} missing ref target(s): {}",
                orphan_ref_names.len(),
                orphan_ref_names.join(", ")
            ),
            kind: BootCheckFindingKind::OrphanRefs(orphan_ref_names),
        });
    }
    findings
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
    let flock = RepoFlock::acquire(&canonical)
        .map_err(|error| format!("acquire repo lock {}: {error}", canonical.display()))?;
    if !flock.is_real() {
        return Err("auto repair refused: repository lock is not held (phantom lock)".to_string());
    }

    let backup_path = write_ref_backup(root, candidate, refs)?;
    actions.push(format!("backup_refs:{}", backup_path.display()));

    let mut pruned_refs = Vec::new();
    for finding in safe_refs {
        match prune_missing_ref(&candidate.path, finding, false)
            .map_err(|error| format!("revalidate/prune {}: {error}", finding.ref_name))?
        {
            PruneRefOutcome::Pruned => {
                actions.push(format!("prune_ref:{}", finding.ref_name));
                pruned_refs.push(finding.ref_name.clone());
            }
            outcome => {
                actions.push(format!("skip_ref:{}:{outcome:?}", finding.ref_name));
            }
        }
    }

    if !pruned_refs.is_empty() {
        repack_refs(root, candidate)?;
        actions.push("repack_refs".to_string());
    }

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
        .map_or(0, |duration| duration.as_micros());
    let backup_path = root
        .join("backups")
        .join("refs")
        .join(safe_backup_project_name(&candidate.project))
        .join(format!("{ts}.txt"));
    ref_backup::write_snapshot(&candidate.path, &backup_path, refs)
        .map_err(|error| format!("write complete ref backup {}: {error}", backup_path.display()))?;
    Ok(backup_path)
}

fn repack_refs(root: &Path, candidate: &ArchiveRepoCandidate) -> Result<(), String> {
    // Linked worktrees share packed-refs in the common Git directory, not in
    // their individual worktree admin directories.
    let packed_refs = Repository::open(&candidate.path)
        .map_err(|error| format!("open repository for packed-refs backup: {error}"))?
        .commondir()
        .join("packed-refs");
    let has_packed_refs = match fs::symlink_metadata(&packed_refs) {
        Ok(metadata) if metadata.is_file() => true,
        Ok(_) => return Err(format!("packed-refs is not a regular file: {}", packed_refs.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("inspect packed-refs {}: {error}", packed_refs.display())),
    };
    if has_packed_refs {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_micros());
        let backup_path = root
            .join("backups")
            .join("refs")
            .join(safe_backup_project_name(&candidate.project))
            .join(format!("{ts}-packed-refs.txt"));
        ref_backup::copy_file(&packed_refs, &backup_path).map_err(|error| {
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
    fn preflight_detects_dangling_branch_refs_in_archive_root() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());
        let fake = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        std::fs::write(
            tmp.path().join(".git/refs/heads/crash-recovery"),
            format!("{fake}\n"),
        )
        .unwrap();

        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::Warn);

        assert_eq!(report.total_projects, 1);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].project, ARCHIVE_ROOT_LABEL);
        assert_eq!(
            report.findings[0].kind,
            BootCheckFindingKind::DanglingBranch("refs/heads/crash-recovery".to_string())
        );
        assert!(
            report.findings[0]
                .detail
                .contains("dangling branch ref target missing")
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
            backups.iter().any(|path| std::fs::read_to_string(path).is_ok_and(|text| {
                text.contains("orphan  refs/stash")
                    && text.contains("symref  HEAD  refs/heads/")
                    && text.contains(&format!("ref  refs/stash  {fake}\n"))
                    && text.ends_with("# END agent-mail ref backup\n")
            })),
            "expected complete pre-repair ref backup in {backups:?}"
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
    fn preflight_timeout_exceeded_escalates_to_abort() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());

        let report = preflight_archive_integrity_with_timeout(
            tmp.path(),
            BootCheckMode::Warn,
            Duration::ZERO,
        );

        assert_eq!(report.mode, BootCheckMode::Abort);
        assert!(report.has_findings());
        assert!(report.should_abort());
        assert_eq!(report.findings[0].project, ARCHIVE_ROOT_LABEL);
        assert!(matches!(
            report.findings[0].kind,
            BootCheckFindingKind::TimeoutExceeded { timeout_ms: 0, .. }
        ));
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

    fn stale_stash_fixture(tmp: &TempDir) -> (Repository, ArchiveRepoCandidate, Vec<PrunableRef>) {
        let repo = init_repo_with_commit(tmp.path());
        fs::write(
            repo.path().join("refs/stash"),
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
        )
        .unwrap();
        let refs = detect_missing_refs(tmp.path()).unwrap();
        assert_eq!(refs.len(), 1);
        let candidate = ArchiveRepoCandidate {
            project: ARCHIVE_ROOT_LABEL.to_string(),
            path: tmp.path().to_path_buf(),
        };
        (repo, candidate, refs)
    }

    #[test]
    fn auto_repair_preserves_a_healthy_ref_from_a_stale_scan() {
        let tmp = TempDir::new().unwrap();
        let (repo, candidate, refs) = stale_stash_fixture(&tmp);
        let healthy = repo.head().unwrap().target().unwrap();
        repo.reference("refs/stash", healthy, true, "concurrent repair")
            .unwrap();

        let outcome = auto_repair_missing_refs(tmp.path(), &candidate, &refs).unwrap();

        assert!(outcome.pruned_refs.is_empty());
        assert!(outcome.after_refs.is_empty());
        assert!(!outcome.actions.iter().any(|action| action == "repack_refs"));
        assert!(
            outcome
                .actions
                .iter()
                .any(|action| action.starts_with("skip_ref:"))
        );
        assert_eq!(
            repo.find_reference("refs/stash").unwrap().target(),
            Some(healthy)
        );
    }

    #[test]
    fn auto_repair_preserves_a_different_missing_target_from_a_stale_scan() {
        let tmp = TempDir::new().unwrap();
        let (repo, candidate, refs) = stale_stash_fixture(&tmp);
        let changed = "cafebabecafebabecafebabecafebabecafebabe\n";
        fs::write(repo.path().join("refs/stash"), changed).unwrap();

        let outcome = auto_repair_missing_refs(tmp.path(), &candidate, &refs).unwrap();

        assert!(outcome.pruned_refs.is_empty());
        assert_eq!(outcome.after_refs, vec!["refs/stash".to_string()]);
        assert!(!outcome.actions.iter().any(|action| action == "repack_refs"));
        assert_eq!(
            fs::read_to_string(repo.path().join("refs/stash")).unwrap(),
            changed
        );
    }

    #[test]
    fn auto_repair_refuses_repo_lock_failure_before_backing_up_or_pruning() {
        let tmp = TempDir::new().unwrap();
        let (repo, candidate, refs) = stale_stash_fixture(&tmp);
        let sentinel = mcp_agent_mail_core::git_lock::sentinel_path(tmp.path()).unwrap();
        fs::create_dir(sentinel).unwrap();
        let before = fs::read(repo.path().join("refs/stash")).unwrap();

        assert!(auto_repair_missing_refs(tmp.path(), &candidate, &refs).is_err());

        assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), before);
        assert!(backup_files(tmp.path(), ARCHIVE_ROOT_LABEL).is_empty());
    }

    #[test]
    fn auto_repair_refuses_a_git_writer_holding_the_ref_lock() {
        let tmp = TempDir::new().unwrap();
        let (repo, candidate, refs) = stale_stash_fixture(&tmp);
        let mut writer = repo.transaction().unwrap();
        writer.lock_ref("refs/stash").unwrap();
        let before = fs::read(repo.path().join("refs/stash")).unwrap();

        assert!(auto_repair_missing_refs(tmp.path(), &candidate, &refs).is_err());

        assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), before);
        assert!(!backup_files(tmp.path(), ARCHIVE_ROOT_LABEL).is_empty());
    }

    #[test]
    fn auto_repair_backup_failure_leaves_refs_untouched() {
        let tmp = TempDir::new().unwrap();
        let (repo, candidate, refs) = stale_stash_fixture(&tmp);
        fs::write(tmp.path().join("backups"), b"not a directory").unwrap();
        let before = fs::read(repo.path().join("refs/stash")).unwrap();

        assert!(auto_repair_missing_refs(tmp.path(), &candidate, &refs).is_err());

        assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), before);
    }

    #[test]
    fn preflight_corrupt_object_is_never_healthy_or_auto_pruned() {
        let tmp = TempDir::new().unwrap();
        let (repo, _, refs) = stale_stash_fixture(&tmp);
        let oid = &refs[0].target_sha;
        let directory = repo.path().join("objects").join(&oid[..2]);
        fs::create_dir_all(&directory).unwrap();
        let object = directory.join(&oid[2..]);
        fs::write(&object, b"not a zlib object").unwrap();
        let before = fs::read(repo.path().join("refs/stash")).unwrap();

        for mode in [BootCheckMode::Warn, BootCheckMode::Abort, BootCheckMode::AutoRepair] {
            let report = preflight_archive_integrity(tmp.path(), mode);
            assert!(report.has_findings());
            assert_eq!(report.findings[0].kind, BootCheckFindingKind::RepoBroken);
            assert_eq!(report.auto_repaired_count, 0);
            assert_eq!(report.should_abort(), mode == BootCheckMode::Abort);
        }
        assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), before);
        assert_eq!(fs::read(object).unwrap(), b"not a zlib object");
        assert!(backup_files(tmp.path(), ARCHIVE_ROOT_LABEL).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn auto_repair_refuses_symlinked_backup_authority_before_pruning() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let (repo, candidate, refs) = stale_stash_fixture(&tmp);
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("backups")).unwrap();
        let before = fs::read(repo.path().join("refs/stash")).unwrap();

        assert!(auto_repair_missing_refs(tmp.path(), &candidate, &refs).is_err());
        assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), before);
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn repack_refuses_nonregular_packed_refs_without_touching_them() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let packed = repo.commondir().join("packed-refs");
        fs::create_dir(&packed).unwrap();
        let candidate = ArchiveRepoCandidate {
            project: ARCHIVE_ROOT_LABEL.to_string(),
            path: tmp.path().to_path_buf(),
        };

        assert!(repack_refs(tmp.path(), &candidate).is_err());
        assert!(packed.is_dir());
        assert!(backup_files(tmp.path(), ARCHIVE_ROOT_LABEL).is_empty());
    }
}
