//! Boot-time archive integrity preflight.
//!
//! The default path is read-only: it discovers archive git repositories,
//! checks whether they open cleanly, and reuses the git-2.51 recovery detector
//! for missing-ref findings. `AutoRepair` is intentionally narrower than the
//! detector: it writes a backup first, then prunes only refs already classified
//! by the recovery layer as safe-to-prune. Every deletion revalidates the
//! original target under the Git ref lock; a stale scan cannot delete a repair.
//! Discovery failures are findings, not evidence of an empty archive. An
//! incomplete discovery never authorizes automatic repair of a partial list.
//! Repair retains mutex-before-flock ordering and the original boot deadline.
//! A partial repair retains its backup and confirmed actions without claiming
//! that an interrupted post-repair scan established a clean repository.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{SecondsFormat, Utc};
use git2::Repository;
use mcp_agent_mail_core::git_lock::{
    DEFAULT_FLOCK_TIMEOUT_SECS, GitRepoLocks, RepoFlock, canonicalize_repo,
};
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
/// Count every directory entry, including files and non-repository directories.
/// Reaching this bound is an incomplete scan, never a clean truncated result.
const MAX_PROJECT_ENTRIES: usize = 16_384;

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
    /// Discovered candidates, not a claim of complete coverage when findings exist.
    pub total_projects: u32,
    pub findings: Vec<BootCheckFinding>,
    /// Repositories with confirmed prunes, including partial repairs with findings.
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

#[derive(Debug, Default)]
struct CandidateDiscovery {
    candidates: Vec<ArchiveRepoCandidate>,
    findings: Vec<BootCheckFinding>,
}

impl CandidateDiscovery {
    fn record_error(&mut self, project: &str, path: &Path, error: impl std::fmt::Display) {
        self.findings.push(BootCheckFinding {
            project: project.to_string(),
            kind: BootCheckFindingKind::RepoBroken,
            detail: format!(
                "archive discovery incomplete at {}: {error}",
                path.display()
            ),
        });
    }

    fn consider(&mut self, project: String, path: PathBuf) {
        match has_git_metadata(&path) {
            Ok(true) => self.candidates.push(ArchiveRepoCandidate { project, path }),
            Ok(false) => {}
            Err(error) => self.record_error(&project, &path, error),
        }
    }

    fn check_timeout(&mut self, root: &Path, timer: Instant, timeout: Duration) -> bool {
        let Some(finding) = timeout_finding_if_exceeded(root, timer.elapsed(), timeout) else {
            return false;
        };
        if !self
            .findings
            .iter()
            .any(|finding| matches!(finding.kind, BootCheckFindingKind::TimeoutExceeded { .. }))
        {
            self.findings.push(finding);
        }
        true
    }

    fn finish(mut self, root: &Path, timer: Instant, timeout: Duration) -> Self {
        self.candidates
            .sort_by(|left, right| left.path.cmp(&right.path));
        self.check_timeout(root, timer, timeout);
        self
    }
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
    after_verified: bool,
    pruned_refs: Vec<String>,
}

#[derive(Debug)]
struct AutoRepairFailure {
    detail: String,
    progress: AutoRepairOutcome,
}

impl std::fmt::Display for AutoRepairFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.detail.fmt(formatter)
    }
}

/// Read-only by default. Discovery is bounded and failed discovery blocks repair.
#[must_use]
pub fn preflight_archive_integrity(root: &Path, mode: BootCheckMode) -> BootCheckReport {
    preflight_archive_integrity_with_timeout(root, mode, BOOT_CHECK_TIMEOUT, MAX_PROJECT_ENTRIES)
}

fn preflight_archive_integrity_with_timeout(
    root: &Path,
    mode: BootCheckMode,
    timeout: Duration,
    max_project_entries: usize,
) -> BootCheckReport {
    let started = Utc::now();
    let started_at = started.to_rfc3339_opts(SecondsFormat::Micros, true);
    let timer = Instant::now();
    let repo_slug = mcp_agent_mail_core::slugify(&root.display().to_string());
    let args_hash = boot_check_args_hash(root, mode);
    let discovery = archive_repo_candidates(root, timer, timeout, max_project_entries);
    let total_projects = u32::try_from(discovery.candidates.len()).unwrap_or(u32::MAX);
    // A partial list cannot grant mutation authority, even for an apparently
    // repairable candidate discovered before another directory failed.
    let allow_auto_repair = mode == BootCheckMode::AutoRepair && discovery.findings.is_empty();
    let mut findings = discovery.findings;
    let candidates = discovery.candidates;
    let discovery_timed_out = findings
        .iter()
        .find(|finding| matches!(finding.kind, BootCheckFindingKind::TimeoutExceeded { .. }));
    let mut effective_mode = mode;
    if let Some(finding) = discovery_timed_out {
        effective_mode = BootCheckMode::Abort;
        emit_timeout_exceeded(
            &repo_slug,
            &args_hash,
            timeout_finding_elapsed_ms(finding),
            duration_ms(timeout),
            total_projects,
            0,
        );
    }
    let discovery_timed_out = discovery_timed_out.is_some();

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

    let mut auto_repaired_count = 0_u32;
    for (index, candidate) in candidates.iter().enumerate() {
        if discovery_timed_out {
            break;
        }
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
                if !allow_auto_repair {
                    findings.extend(missing_ref_findings);
                } else {
                    // Every candidate shares the original preflight clock. A
                    // slow scan or lock wait cannot grant a fresh repair budget.
                    let mut remaining = |stage: &str| boot_check_remaining(timer, timeout, stage);
                    match auto_repair_missing_refs(root, candidate, &refs, &mut remaining) {
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
                        Err(failure) => {
                            emit_auto_repair_failed(
                                &repo_slug,
                                &args_hash,
                                mode,
                                candidate,
                                &failure.detail,
                            );
                            record_auto_repair_evidence(candidate, &failure.progress);
                            if !failure.progress.pruned_refs.is_empty() {
                                auto_repaired_count = auto_repaired_count.saturating_add(1);
                            }
                            findings.push(BootCheckFinding {
                                project: candidate.project.clone(),
                                kind: BootCheckFindingKind::RepoBroken,
                                detail: format!(
                                    "auto repair stopped: {failure}; confirmed prunes: {:?}; \
                                     backup: {:?}; post-repair scan complete: {}",
                                    failure.progress.pruned_refs,
                                    failure.progress.backup_path,
                                    failure.progress.after_verified,
                                ),
                            });
                        }
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
    // Empty candidate sets and the final read-only candidate must not bypass
    // expiry. Do not emit a second timeout for an already-incomplete scan.
    if !findings
        .iter()
        .any(|finding| matches!(finding.kind, BootCheckFindingKind::TimeoutExceeded { .. }))
        && let Some(finding) = timeout_finding_if_exceeded(root, timer.elapsed(), timeout)
    {
        effective_mode = BootCheckMode::Abort;
        emit_timeout_exceeded(
            &repo_slug,
            &args_hash,
            timeout_finding_elapsed_ms(&finding),
            duration_ms(timeout),
            total_projects,
            total_projects,
        );
        findings.push(finding);
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

fn archive_repo_candidates(
    root: &Path,
    timer: Instant,
    timeout: Duration,
    max_entries: usize,
) -> CandidateDiscovery {
    let mut discovery = CandidateDiscovery::default();
    if discovery.check_timeout(root, timer, timeout) {
        return discovery;
    }
    match nonsymlink_directory_exists(root) {
        Ok(true) => discovery.consider(ARCHIVE_ROOT_LABEL.to_string(), root.to_path_buf()),
        Ok(false) => return discovery.finish(root, timer, timeout),
        Err(error) => {
            discovery.record_error(ARCHIVE_ROOT_LABEL, root, error);
            return discovery.finish(root, timer, timeout);
        }
    }

    let projects = root.join("projects");
    match nonsymlink_directory_exists(&projects) {
        Ok(true) => {}
        Ok(false) => return discovery.finish(root, timer, timeout),
        Err(error) => {
            discovery.record_error(ARCHIVE_ROOT_LABEL, &projects, error);
            return discovery.finish(root, timer, timeout);
        }
    }
    let entries = match fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) => {
            discovery.record_error(ARCHIVE_ROOT_LABEL, &projects, error);
            return discovery.finish(root, timer, timeout);
        }
    };
    for (index, entry) in entries.enumerate() {
        if discovery.check_timeout(root, timer, timeout) {
            break;
        }
        if index >= max_entries {
            discovery.record_error(
                ARCHIVE_ROOT_LABEL,
                &projects,
                format!("project entry budget exhausted (limit {max_entries}); scan is incomplete"),
            );
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                discovery.record_error(ARCHIVE_ROOT_LABEL, &projects, error);
                continue;
            }
        };
        let path = entry.path();
        let project = entry.file_name().to_string_lossy().into_owned();
        // Re-observe the path instead of trusting a cached d_type after another
        // process replaced the entry. These are observations, not an atomic
        // filesystem snapshot; the scan never claims protection from all races.
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                discovery.record_error(&project, &path, "project entry is a symlink; not followed");
            }
            Ok(metadata) if metadata.is_dir() => discovery.consider(project, path),
            Ok(_) => {}
            Err(error) => discovery.record_error(&project, &path, error),
        }
    }
    discovery.finish(root, timer, timeout)
}

fn nonsymlink_directory_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a non-symlink directory; path was not traversed",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn has_git_metadata(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path.join(".git")) {
        Ok(metadata) if metadata.is_dir() || metadata.is_file() => return Ok(true),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Git metadata is not a regular file or directory; path was not followed",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // Either bare-repository marker warrants inspection. A missing companion
    // is a damaged candidate, not grounds to silently ignore the repository.
    let mut present = false;
    for (name, directory) in [("HEAD", false), ("objects", true)] {
        match fs::symlink_metadata(path.join(name)) {
            Ok(metadata)
                if (directory && metadata.is_dir()) || (!directory && metadata.is_file()) =>
            {
                present = true;
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bare Git marker {name} has an unsupported file type"),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(present)
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

fn boot_check_remaining(
    timer: Instant,
    timeout: Duration,
    stage: &str,
) -> Result<Duration, String> {
    let remaining = timeout.saturating_sub(timer.elapsed());
    if remaining.is_zero() {
        Err(format!(
            "archive boot check budget exhausted before {stage}"
        ))
    } else {
        Ok(remaining)
    }
}

/// No waiter thread is created. Only the production monotonic-clock callback
/// runs here; the injected stage boundary also supports deterministic tests.
fn lock_boot_repository<'a>(
    mutex: &'a Mutex<()>,
    remaining: &mut impl FnMut(&str) -> Result<Duration, String>,
) -> Result<MutexGuard<'a, ()>, String> {
    loop {
        remaining("repository mutex")?;
        let guard = match mutex.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(remaining("repository mutex")?.min(Duration::from_millis(5)));
                continue;
            }
        };
        // A late acquisition does not authorize any later filesystem work.
        remaining("repository mutex")?;
        return Ok(guard);
    }
}

fn validate_repair_candidate(root: &Path, candidate: &ArchiveRepoCandidate) -> Result<(), String> {
    if !nonsymlink_directory_exists(root).map_err(|error| error.to_string())? {
        return Err("archive root disappeared before repair".to_string());
    }
    if candidate.path != root {
        let projects = root.join("projects");
        if candidate.path.parent() != Some(projects.as_path()) {
            return Err("repair candidate is outside the discovered project container".to_string());
        }
        if !nonsymlink_directory_exists(&projects).map_err(|error| error.to_string())? {
            return Err("project container disappeared before repair".to_string());
        }
    }
    if !nonsymlink_directory_exists(&candidate.path).map_err(|error| error.to_string())? {
        return Err("repository directory disappeared before repair".to_string());
    }
    if !has_git_metadata(&candidate.path).map_err(|error| error.to_string())? {
        return Err("Git metadata disappeared before repair".to_string());
    }
    Ok(())
}

/// The callback must return positive remaining time or an error. Production
/// captures the original preflight Instant; it never restarts between stages.
fn auto_repair_missing_refs(
    root: &Path,
    candidate: &ArchiveRepoCandidate,
    refs: &[PrunableRef],
    remaining: &mut impl FnMut(&str) -> Result<Duration, String>,
) -> Result<AutoRepairOutcome, AutoRepairFailure> {
    let before_refs = refs
        .iter()
        .map(|finding| finding.ref_name.clone())
        .collect::<Vec<_>>();
    let safe_refs = refs
        .iter()
        .filter(|finding| finding.category == RefCategory::SafeToPrune)
        .collect::<Vec<_>>();
    let mut progress = AutoRepairOutcome {
        actions: Vec::new(),
        backup_path: None,
        before_refs: before_refs.clone(),
        after_refs: before_refs,
        after_verified: false,
        pruned_refs: Vec::new(),
    };
    let result = (|| -> Result<(), String> {
        remaining("repair admission")?;
        if safe_refs.is_empty() {
            progress.actions.push("refused_no_safe_refs".to_string());
            return Ok(());
        }

        validate_repair_candidate(root, candidate)?;
        remaining("repository resolution")?;
        let canonical = canonicalize_repo(&candidate.path)
            .ok_or_else(|| format!("canonicalize repo {}", candidate.path.display()))?;
        // Same hierarchy as GitCmd: registry -> repository mutex -> flock.
        // Taking flock first creates a cycle with a normal command waiting on it.
        let mutex = GitRepoLocks::global()
            .lock_for_with_timeout(&canonical, remaining("repository lock lookup")?)
            .map_err(|error| format!("repository lock lookup: {error}"))?;
        let _mutex_guard = lock_boot_repository(&mutex, remaining)?;
        let configured_flock_secs = std::env::var("AM_GIT_FLOCK_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_FLOCK_TIMEOUT_SECS);
        let flock_wait =
            remaining("repository flock")?.min(Duration::from_secs(configured_flock_secs));
        validate_repair_candidate(root, candidate)?;
        let flock = RepoFlock::acquire_with_timeout(&canonical, flock_wait)
            .map_err(|error| format!("acquire repo lock {}: {error}", canonical.display()))?;
        if !flock.is_real() {
            return Err(
                "auto repair refused: repository lock is not held (phantom lock)".to_string(),
            );
        }

        remaining("reference backup")?;
        validate_repair_candidate(root, candidate)?;
        let backup_path = write_ref_backup(root, candidate, refs)?;
        progress
            .actions
            .push(format!("backup_refs:{}", backup_path.display()));
        progress.backup_path = Some(backup_path);

        for finding in safe_refs {
            // A completed backup does not extend the deadline. Preserve it if
            // the next destructive transaction can no longer be admitted.
            remaining("reference prune")?;
            match prune_missing_ref(&candidate.path, finding, false)
                .map_err(|error| format!("revalidate/prune {}: {error}", finding.ref_name))?
            {
                PruneRefOutcome::Pruned => {
                    progress
                        .actions
                        .push(format!("prune_ref:{}", finding.ref_name));
                    progress.pruned_refs.push(finding.ref_name.clone());
                }
                outcome => {
                    progress
                        .actions
                        .push(format!("skip_ref:{}:{outcome:?}", finding.ref_name));
                }
            }
        }

        if !progress.pruned_refs.is_empty() {
            repack_refs(root, candidate, remaining)?;
            progress.actions.push("repack_refs".to_string());
        }

        remaining("post-repair scan")?;
        let after = detect_missing_refs(&candidate.path)
            .map_err(|error| format!("post-repair missing-ref scan failed: {error}"))?;
        progress.after_refs = after.into_iter().map(|finding| finding.ref_name).collect();
        progress.after_verified = true;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(progress),
        Err(detail) => {
            progress.actions.push(format!("repair_stopped:{detail}"));
            Err(AutoRepairFailure { detail, progress })
        }
    }
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
    ref_backup::write_snapshot(&candidate.path, &backup_path, refs).map_err(|error| {
        format!(
            "write complete ref backup {}: {error}",
            backup_path.display()
        )
    })?;
    Ok(backup_path)
}

/// Called with both repository mutex and real flock retained by auto repair.
fn repack_refs(
    root: &Path,
    candidate: &ArchiveRepoCandidate,
    remaining: &mut impl FnMut(&str) -> Result<Duration, String>,
) -> Result<(), String> {
    remaining("packed-ref inspection")?;
    // Linked worktrees share packed-refs in the common Git directory, not in
    // their individual worktree admin directories.
    let packed_refs = Repository::open(&candidate.path)
        .map_err(|error| format!("open repository for packed-refs backup: {error}"))?
        .commondir()
        .join("packed-refs");
    let has_packed_refs = match fs::symlink_metadata(&packed_refs) {
        Ok(metadata) if metadata.is_file() => true,
        Ok(_) => {
            return Err(format!(
                "packed-refs is not a regular file: {}",
                packed_refs.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "inspect packed-refs {}: {error}",
                packed_refs.display()
            ));
        }
    };
    if has_packed_refs {
        remaining("packed-ref backup")?;
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
        // Both layers are already owned in their normal acquisition order.
        // Reacquiring the mutex here would deadlock with this very repair.
        .skip_flock()
        .skip_mutex()
        .timeout(remaining("reference repack")?)
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

fn repair_after_state(outcome: &AutoRepairOutcome) -> serde_json::Value {
    json!({
        "missing_refs": outcome.after_verified.then_some(&outcome.after_refs),
        "verified": outcome.after_verified,
    })
}

fn emit_auto_repair_attempted(
    repo_slug: &str,
    args_hash: &str,
    mode: BootCheckMode,
    candidate: &ArchiveRepoCandidate,
    outcome: &AutoRepairOutcome,
) {
    let before_state = json!({ "missing_refs": outcome.before_refs });
    let after_state = repair_after_state(outcome);
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
        "after_state": repair_after_state(outcome),
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

    fn repair_budget() -> impl FnMut(&str) -> Result<Duration, String> {
        let timer = Instant::now();
        move |stage| boot_check_remaining(timer, BOOT_CHECK_TIMEOUT, stage)
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
            backups
                .iter()
                .any(|path| std::fs::read_to_string(path).is_ok_and(|text| {
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
            MAX_PROJECT_ENTRIES,
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
    fn preflight_skips_symlinked_project_repos_and_reports_incomplete_coverage() {
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
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].kind, BootCheckFindingKind::RepoBroken);
        assert!(report.findings[0].detail.contains("not followed"));
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

        let outcome =
            auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut repair_budget()).unwrap();

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

        let outcome =
            auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut repair_budget()).unwrap();

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

        assert!(
            auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut repair_budget()).is_err()
        );

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

        assert!(
            auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut repair_budget()).is_err()
        );

        assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), before);
        assert!(!backup_files(tmp.path(), ARCHIVE_ROOT_LABEL).is_empty());
    }

    #[test]
    fn auto_repair_backup_failure_leaves_refs_untouched() {
        let tmp = TempDir::new().unwrap();
        let (repo, candidate, refs) = stale_stash_fixture(&tmp);
        fs::write(tmp.path().join("backups"), b"not a directory").unwrap();
        let before = fs::read(repo.path().join("refs/stash")).unwrap();

        assert!(
            auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut repair_budget()).is_err()
        );

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

        for mode in [
            BootCheckMode::Warn,
            BootCheckMode::Abort,
            BootCheckMode::AutoRepair,
        ] {
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

        assert!(
            auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut repair_budget()).is_err()
        );
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

        assert!(repack_refs(tmp.path(), &candidate, &mut repair_budget()).is_err());
        assert!(packed.is_dir());
        assert!(backup_files(tmp.path(), ARCHIVE_ROOT_LABEL).is_empty());
    }

    #[test]
    fn empty_or_missing_scan_cannot_bypass_zero_budget() {
        let tmp = TempDir::new().unwrap();
        for root in [tmp.path().to_path_buf(), tmp.path().join("absent")] {
            let report = preflight_archive_integrity_with_timeout(
                &root,
                BootCheckMode::Warn,
                Duration::ZERO,
                MAX_PROJECT_ENTRIES,
            );
            assert_eq!(report.total_projects, 0);
            assert!(report.should_abort());
            assert_eq!(report.findings.len(), 1);
            assert!(matches!(
                report.findings[0].kind,
                BootCheckFindingKind::TimeoutExceeded { .. }
            ));
        }
    }

    #[test]
    fn non_directory_archive_root_is_not_a_clean_empty_archive() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("archive");
        fs::write(&root, b"preserve").unwrap();
        let report = preflight_archive_integrity(&root, BootCheckMode::Abort);
        assert!(report.should_abort());
        assert_eq!(report.findings[0].kind, BootCheckFindingKind::RepoBroken);
        assert_eq!(fs::read(&root).unwrap(), b"preserve");
    }

    #[test]
    fn failed_project_container_blocks_repair_of_discovered_root() {
        let tmp = TempDir::new().unwrap();
        let (repo, _, _) = stale_stash_fixture(&tmp);
        let before = fs::read(repo.path().join("refs/stash")).unwrap();
        fs::write(tmp.path().join("projects"), b"not a directory").unwrap();
        for mode in [
            BootCheckMode::Warn,
            BootCheckMode::Abort,
            BootCheckMode::AutoRepair,
        ] {
            let report = preflight_archive_integrity(tmp.path(), mode);
            assert_eq!(report.total_projects, 1);
            assert!(
                report
                    .findings
                    .iter()
                    .any(|finding| finding.detail.contains("discovery incomplete"))
            );
            assert_eq!(report.auto_repaired_count, 0);
            assert_eq!(report.should_abort(), mode == BootCheckMode::Abort);
            assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), before);
            assert!(backup_files(tmp.path(), ARCHIVE_ROOT_LABEL).is_empty());
        }
    }

    #[test]
    fn project_entry_budget_counts_non_repositories_and_blocks_partial_repair() {
        let tmp = TempDir::new().unwrap();
        let (repo, _, _) = stale_stash_fixture(&tmp);
        let before = fs::read(repo.path().join("refs/stash")).unwrap();
        let projects = tmp.path().join("projects");
        fs::create_dir(&projects).unwrap();
        for index in 0..4 {
            fs::write(projects.join(format!("ordinary-{index}")), b"data").unwrap();
        }
        let report = preflight_archive_integrity_with_timeout(
            tmp.path(),
            BootCheckMode::AutoRepair,
            BOOT_CHECK_TIMEOUT,
            3,
        );
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.detail.contains("entry budget exhausted"))
        );
        assert_eq!(report.auto_repaired_count, 0);
        assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), before);
        assert!(backup_files(tmp.path(), ARCHIVE_ROOT_LABEL).is_empty());
        let complete = archive_repo_candidates(tmp.path(), Instant::now(), BOOT_CHECK_TIMEOUT, 4);
        assert!(
            complete.findings.is_empty(),
            "the exact entry bound is accepted"
        );
        assert_eq!(complete.candidates.len(), 1);
    }

    #[test]
    fn ordinary_project_directories_remain_valid_and_candidate_order_is_stable() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path().join("projects");
        fs::create_dir_all(projects.join("ordinary")).unwrap();
        fs::write(projects.join("README"), b"not a repo").unwrap();
        for name in ["zeta", "alpha"] {
            init_repo_with_commit(&projects.join(name));
        }
        let discovery = archive_repo_candidates(tmp.path(), Instant::now(), BOOT_CHECK_TIMEOUT, 4);
        assert!(discovery.findings.is_empty());
        assert_eq!(
            discovery
                .candidates
                .iter()
                .map(|candidate| candidate.project.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::Abort);
        assert_eq!(report.total_projects, 2);
        assert!(!report.has_findings());
    }

    #[test]
    fn partial_bare_repository_is_inspected_instead_of_ignored() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::Abort);
        assert_eq!(report.total_projects, 1);
        assert!(report.should_abort());
        assert_eq!(report.findings[0].kind, BootCheckFindingKind::RepoBroken);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_project_container_cannot_redirect_auto_repair() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let (root_repo, _, _) = stale_stash_fixture(&tmp);
        let foreign = outside.path().join("foreign");
        let repo = init_repo_with_commit(&foreign);
        let missing = b"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n";
        fs::write(repo.path().join("refs/stash"), missing).unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("projects")).unwrap();
        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::AutoRepair);
        assert_eq!(
            report.total_projects, 1,
            "foreign repository must not be visited"
        );
        assert!(report.has_findings());
        assert_eq!(report.auto_repaired_count, 0);
        assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), missing);
        assert_eq!(
            fs::read(root_repo.path().join("refs/stash")).unwrap(),
            missing
        );
        assert!(!tmp.path().join("backups").exists());
        assert!(!outside.path().join("backups").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_archive_root_is_reported_without_traversing_projects() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        init_repo_with_commit(&outside.path().join("projects/foreign"));
        let linked = tmp.path().join("archive");
        std::os::unix::fs::symlink(outside.path(), &linked).unwrap();
        let report = preflight_archive_integrity(&linked, BootCheckMode::Abort);
        assert_eq!(report.total_projects, 0);
        assert!(report.should_abort());
        assert_eq!(report.findings.len(), 1);
        assert!(!outside.path().join("backups").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_git_metadata_is_not_silently_treated_as_non_repository() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let repo = init_repo_with_commit(outside.path());
        std::os::unix::fs::symlink(repo.path(), tmp.path().join(".git")).unwrap();
        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::Abort);
        assert_eq!(report.total_projects, 0);
        assert!(report.should_abort());
        assert!(report.findings[0].detail.contains("Git metadata"));
        assert!(!tmp.path().join("backups").exists());
    }

    #[test]
    fn expired_repair_admission_creates_neither_lock_nor_backup() {
        let tmp = TempDir::new().unwrap();
        let (repo, candidate, refs) = stale_stash_fixture(&tmp);
        let before = fs::read(repo.path().join("refs/stash")).unwrap();
        let timer = Instant::now();
        let failure = auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut |stage| {
            boot_check_remaining(timer, Duration::ZERO, stage)
        })
        .unwrap_err();
        assert!(failure.detail.contains("repair admission"));
        assert!(failure.progress.backup_path.is_none());
        assert!(failure.progress.pruned_refs.is_empty());
        assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), before);
        assert!(!repo.path().join("am.git-serialize.lock").exists());
        assert!(!tmp.path().join("backups").exists());
    }

    #[test]
    fn deadline_after_backup_preserves_complete_backup_without_pruning() {
        let tmp = TempDir::new().unwrap();
        let (repo, candidate, refs) = stale_stash_fixture(&tmp);
        let before = fs::read(repo.path().join("refs/stash")).unwrap();
        let failure = auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut |stage| {
            if stage == "reference prune" {
                Err("deadline after complete backup".to_string())
            } else {
                Ok(BOOT_CHECK_TIMEOUT)
            }
        })
        .unwrap_err();
        let backup = failure.progress.backup_path.as_ref().unwrap();
        assert!(
            fs::read_to_string(backup)
                .unwrap()
                .ends_with("# END agent-mail ref backup\n")
        );
        assert!(failure.progress.pruned_refs.is_empty());
        assert!(!failure.progress.after_verified);
        assert_eq!(
            repair_after_state(&failure.progress)["missing_refs"],
            serde_json::Value::Null
        );
        assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), before);
        assert!(
            RepoFlock::acquire_with_timeout(tmp.path(), Duration::ZERO)
                .unwrap()
                .is_real()
        );
    }

    fn two_stale_refs(tmp: &TempDir) -> (Repository, ArchiveRepoCandidate, Vec<PrunableRef>) {
        let (repo, candidate, _) = stale_stash_fixture(tmp);
        fs::create_dir_all(repo.path().join("refs/temp")).unwrap();
        fs::write(
            repo.path().join("refs/temp/pending"),
            b"cafebabecafebabecafebabecafebabecafebabe\n",
        )
        .unwrap();
        let mut refs = detect_missing_refs(tmp.path()).unwrap();
        refs.sort_by(|left, right| left.ref_name.cmp(&right.ref_name));
        assert_eq!(refs.len(), 2);
        (repo, candidate, refs)
    }

    #[test]
    fn partial_deadline_failure_retains_confirmed_prunes_and_can_be_retried() {
        let tmp = TempDir::new().unwrap();
        let (repo, candidate, refs) = two_stale_refs(&tmp);
        let remaining_before = fs::read(repo.path().join(&refs[1].ref_name)).unwrap();
        let mut prune_admissions = 0;
        let failure = auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut |stage| {
            if stage == "reference prune" {
                prune_admissions += 1;
                if prune_admissions == 2 {
                    return Err("deadline before second prune".to_string());
                }
            }
            Ok(BOOT_CHECK_TIMEOUT)
        })
        .unwrap_err();
        assert_eq!(failure.progress.pruned_refs, vec![refs[0].ref_name.clone()]);
        assert!(failure.progress.backup_path.as_ref().unwrap().is_file());
        assert!(!failure.progress.after_verified);
        assert!(repo.find_reference(&refs[0].ref_name).is_err());
        assert_eq!(
            fs::read(repo.path().join(&refs[1].ref_name)).unwrap(),
            remaining_before
        );
        let remaining_refs = detect_missing_refs(tmp.path()).unwrap();
        let completed = auto_repair_missing_refs(
            tmp.path(),
            &candidate,
            &remaining_refs,
            &mut repair_budget(),
        )
        .unwrap();
        assert_eq!(completed.pruned_refs, vec![refs[1].ref_name.clone()]);
        assert!(completed.after_verified);
        assert!(completed.after_refs.is_empty());
    }

    #[test]
    fn contended_repository_mutex_cannot_acquire_flock_or_write_backup() {
        let tmp = TempDir::new().unwrap();
        let (repo, candidate, refs) = stale_stash_fixture(&tmp);
        let canonical = canonicalize_repo(tmp.path()).unwrap();
        let mutex = GitRepoLocks::global().lock_for(&canonical);
        let held = mutex.lock().unwrap();
        let mut mutex_checks = 0;
        let failure = auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut |stage| {
            if stage == "repository mutex" {
                mutex_checks += 1;
                if mutex_checks == 2 {
                    return Err("deadline while waiting for repository mutex".to_string());
                }
            }
            Ok(BOOT_CHECK_TIMEOUT)
        })
        .unwrap_err();
        assert_eq!(
            mutex_checks, 2,
            "one actual try_lock must observe contention"
        );
        assert!(failure.detail.contains("repository mutex"));
        assert!(!repo.path().join("am.git-serialize.lock").exists());
        assert!(!tmp.path().join("backups").exists());
        assert!(repo.path().join("refs/stash").exists());
        drop(held);
        let repaired =
            auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut repair_budget()).unwrap();
        assert_eq!(repaired.pruned_refs.len(), 1);
        assert!(repaired.after_verified);
    }

    #[test]
    fn repair_retains_both_locks_through_backup_prune_and_real_repack() {
        let tmp = TempDir::new().unwrap();
        let (_, candidate, refs) = stale_stash_fixture(&tmp);
        let canonical = canonicalize_repo(tmp.path()).unwrap();
        let mutex = GitRepoLocks::global().lock_for(&canonical);
        let mut observed_stages = Vec::new();
        let mut clock = repair_budget();
        let repaired = auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut |stage| {
            if matches!(
                stage,
                "reference backup" | "reference prune" | "reference repack"
            ) {
                assert!(matches!(mutex.try_lock(), Err(TryLockError::WouldBlock)));
                let error =
                    RepoFlock::acquire_with_timeout(&canonical, Duration::ZERO).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                observed_stages.push(stage.to_string());
            }
            clock(stage)
        })
        .unwrap();
        assert_eq!(
            observed_stages,
            vec!["reference backup", "reference prune", "reference repack"]
        );
        assert!(
            repaired
                .actions
                .iter()
                .any(|action| action == "repack_refs")
        );
        assert!(repaired.after_verified);
        assert!(repaired.after_refs.is_empty());
        assert!(mutex.try_lock().is_ok());
    }

    #[test]
    fn preflight_flock_contention_uses_boot_budget_and_retry_makes_progress() {
        let tmp = TempDir::new().unwrap();
        let (repo, _, _) = stale_stash_fixture(&tmp);
        let canonical = canonicalize_repo(tmp.path()).unwrap();
        let held = RepoFlock::acquire(&canonical).unwrap();
        let before = fs::read(repo.path().join("refs/stash")).unwrap();
        let started = Instant::now();
        let report = preflight_archive_integrity_with_timeout(
            tmp.path(),
            BootCheckMode::AutoRepair,
            Duration::from_millis(100),
            MAX_PROJECT_ENTRIES,
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(report.should_abort());
        assert_eq!(report.auto_repaired_count, 0);
        assert_eq!(fs::read(repo.path().join("refs/stash")).unwrap(), before);
        assert!(backup_files(tmp.path(), ARCHIVE_ROOT_LABEL).is_empty());
        drop(held);
        let repaired = preflight_archive_integrity(tmp.path(), BootCheckMode::AutoRepair);
        assert_eq!(repaired.auto_repaired_count, 1);
        assert!(!repaired.has_findings());
    }

    #[test]
    fn preflight_reports_partial_prunes_when_a_later_ref_is_locked() {
        let tmp = TempDir::new().unwrap();
        let (repo, _, refs) = two_stale_refs(&tmp);
        let mut writer = repo.transaction().unwrap();
        writer.lock_ref(&refs[1].ref_name).unwrap();
        let report = preflight_archive_integrity(tmp.path(), BootCheckMode::AutoRepair);
        assert!(report.has_findings());
        assert_eq!(
            report.auto_repaired_count, 1,
            "confirmed changes must not disappear"
        );
        assert!(report.findings.iter().any(|finding| {
            finding.detail.contains("confirmed prunes")
                && finding.detail.contains(&refs[0].ref_name)
                && finding.detail.contains("post-repair scan complete: false")
        }));
        assert!(repo.find_reference(&refs[0].ref_name).is_err());
        assert!(repo.find_reference(&refs[1].ref_name).is_ok());
        assert!(!backup_files(tmp.path(), ARCHIVE_ROOT_LABEL).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn metadata_redirect_observed_after_mutex_wait_stops_before_flock_or_backup() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let (_, candidate, refs) = stale_stash_fixture(&tmp);
        let foreign = init_repo_with_commit(outside.path());
        let before = fs::read(foreign.path().join("HEAD")).unwrap();
        let original_git = tmp.path().join("original-git");
        let mut swapped = false;
        let failure = auto_repair_missing_refs(tmp.path(), &candidate, &refs, &mut |stage| {
            if stage == "repository flock" && !swapped {
                fs::rename(tmp.path().join(".git"), &original_git).unwrap();
                std::os::unix::fs::symlink(foreign.path(), tmp.path().join(".git")).unwrap();
                swapped = true;
            }
            Ok(BOOT_CHECK_TIMEOUT)
        })
        .unwrap_err();
        assert!(swapped);
        assert!(failure.detail.contains("Git metadata"));
        assert!(failure.progress.backup_path.is_none());
        assert!(failure.progress.pruned_refs.is_empty());
        assert_eq!(fs::read(foreign.path().join("HEAD")).unwrap(), before);
        assert!(original_git.join("refs/stash").is_file());
        assert!(!foreign.path().join("am.git-serialize.lock").exists());
        assert!(!tmp.path().join("backups").exists());
    }

    #[test]
    fn remaining_budget_rejects_expiry_without_instant_overflow() {
        let now = Instant::now();
        assert!(boot_check_remaining(now, Duration::ZERO, "write").is_err());
        assert!(boot_check_remaining(now, Duration::MAX, "write").unwrap() > Duration::ZERO);
        let past = now.checked_sub(Duration::from_secs(1)).unwrap();
        assert!(boot_check_remaining(past, Duration::from_millis(1), "write").is_err());
    }
}
