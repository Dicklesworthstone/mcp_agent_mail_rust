//! Recovery helpers for repos damaged by the git 2.51.0 index-race.
//!
//! See `docs/GIT_251_FINDINGS.md` for background on the bug and
//! `RECOVERY_RUNBOOK.md` for the operator playbook.
//!
//! # What this module does
//!
//! - [`detect_missing_refs`] (br-8ujfs.6.2 / F2): walk every ref in a
//!   repo and identify ones whose target object is missing from the
//!   object database. These are "orphan" refs left behind when a
//!   writer crashed mid-update.
//! - Future: [`prune_orphan_refs`] (F3) and [`repack_refs`] (F4) will
//!   live here once the F3/F4 beads land.
//!
//! # Non-goals
//!
//! - We NEVER touch the working tree. All operations are pure ref /
//!   ODB introspection.
//! - We NEVER delete objects. If a ref points to a missing object we
//!   delete the REF; the (missing) object is already gone.
//! - We NEVER auto-repair. Detection is strictly read-only; the
//!   caller (Track F's `am doctor fix-orphan-refs` command) decides
//!   when to prune.

use std::path::Path;

use git2::{ObjectType, Oid, Repository};

/// A ref that cannot be followed because its target object is missing
/// from the repository's object database.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PrunableRef {
    /// Full ref name, e.g. `refs/stash`, `refs/heads/foo`.
    pub ref_name: String,

    /// The object id the ref was pointing at (peeled through tag
    /// chains if applicable).
    pub target_sha: String,

    /// Short human-readable reason, included in the action log.
    pub reason: String,

    /// True if this ref is in a namespace we consider SAFE to prune
    /// without operator override. See [`ref_category`].
    pub category: RefCategory,
}

/// Classification of refs for the pruning safety gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RefCategory {
    /// Primary refs (main/master/HEAD). Never auto-prune even with
    /// `--force`; operator must intervene manually.
    Protected,
    /// Safe-to-prune namespaces (`refs/stash`, `refs/temp/*`,
    /// `refs/original/*`).
    SafeToPrune,
    /// Everything else (`refs/heads/*`, `refs/tags/*`,
    /// `refs/remotes/*`, custom). Requires `--force`.
    AskUser,
}

/// Classify a ref name into a [`RefCategory`].
///
/// This is the central safety gate for F3 (pruning). Called by both
/// detection (to label findings) and the prune path (to decide whether
/// to proceed).
#[must_use]
pub fn ref_category(ref_name: &str) -> RefCategory {
    // Protected: primary branches + HEAD + their remote tracking.
    const PROTECTED: &[&str] = &[
        "HEAD",
        "refs/heads/main",
        "refs/heads/master",
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
        "refs/remotes/origin/HEAD",
    ];
    if PROTECTED.contains(&ref_name) {
        return RefCategory::Protected;
    }

    // Safe-to-prune namespaces.
    const SAFE_PREFIXES: &[&str] = &[
        "refs/stash",
        "refs/temp/",
        "refs/original/",
    ];
    if ref_name == "refs/stash" {
        return RefCategory::SafeToPrune;
    }
    for prefix in SAFE_PREFIXES {
        if ref_name.starts_with(prefix) {
            return RefCategory::SafeToPrune;
        }
    }

    RefCategory::AskUser
}

/// Detect refs whose target objects are missing from the repo's ODB.
///
/// This is the libgit2-native replacement for the original plan to
/// shell out `git fsck --unreachable --no-reflogs` and parse stderr.
/// Reason for the switch (per bead F2 revision v2):
///
/// - git 2.51.0 itself can segfault during fsck under load — using
///   the binary we're trying to survive is a bad plan.
/// - fsck output format varies between git versions; parsing is
///   brittle.
/// - libgit2 exposes `odb.exists()` and the full ref database; the
///   check is trivial and faster than fsck anyway.
///
/// # Arguments
///
/// - `repo_path`: path to the repo (normal, bare, or worktree).
///
/// # Returns
///
/// Vector of [`PrunableRef`] entries, one per ref with a missing
/// target. Empty vector means the repo's ref integrity is intact.
///
/// # Errors
///
/// Returns `git2::Error` if the repo cannot be opened, or if the
/// references iterator fails. ODB lookups that fail are logged but
/// do not abort — we want to list as many findings as we can.
pub fn detect_missing_refs(repo_path: &Path) -> Result<Vec<PrunableRef>, git2::Error> {
    let repo = Repository::open(repo_path)?;
    let odb = repo.odb()?;
    let mut out = Vec::new();

    let references = repo.references()?;
    for r in references {
        let reference = match r {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "mcp_agent_mail::storage::recovery",
                    err = %e,
                    "recovery_reference_iter_error"
                );
                continue;
            }
        };
        let name = reference.name().unwrap_or("<invalid-utf8>").to_string();

        // Peel through tag chains to get the final ODB object we care
        // about. If the reference is direct, `target()` gives the oid;
        // if it's a tag object chain, `peel()` resolves to the final
        // commit/tree/blob.
        let (peeled_oid, peel_reason): (Option<Oid>, &'static str) =
            if let Ok(obj) = reference.peel(ObjectType::Any) {
                (Some(obj.id()), "peeled")
            } else if let Some(target) = reference.target() {
                (Some(target), "direct-target")
            } else if let Some(sym) = reference.symbolic_target() {
                // Symbolic ref that points to something; peel through
                // one level. If the pointed-to ref doesn't exist we
                // handle that as its own finding (the direct ref).
                tracing::debug!(
                    target: "mcp_agent_mail::storage::recovery",
                    ref = %name,
                    symbolic_target = %sym,
                    "recovery_ref_symbolic_deferred_to_direct_check"
                );
                continue;
            } else {
                (None, "no-target")
            };

        let Some(oid) = peeled_oid else {
            continue;
        };

        match odb.exists(oid) {
            true => {
                tracing::trace!(
                    target: "mcp_agent_mail::storage::recovery",
                    ref = %name,
                    oid = %oid,
                    via = peel_reason,
                    "recovery_ref_intact"
                );
            }
            false => {
                let category = ref_category(&name);
                let finding = PrunableRef {
                    ref_name: name.clone(),
                    target_sha: oid.to_string(),
                    reason: format!("object {oid} missing from ODB (via {peel_reason})"),
                    category,
                };
                tracing::info!(
                    target: "mcp_agent_mail::storage::recovery",
                    ref = %name,
                    oid = %oid,
                    via = peel_reason,
                    category = ?category,
                    "recovery_ref_missing_object"
                );
                out.push(finding);
            }
        }
    }

    Ok(out)
}

/// Summary counts for reporting (matches F1's JSON schema skeleton).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct DetectionSummary {
    pub total_refs_scanned: usize,
    pub findings: usize,
    pub by_category: CategoryCounts,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct CategoryCounts {
    pub protected: usize,
    pub safe_to_prune: usize,
    pub ask_user: usize,
}

impl DetectionSummary {
    #[must_use]
    pub fn from_findings(total_scanned: usize, findings: &[PrunableRef]) -> Self {
        let mut by_category = CategoryCounts::default();
        for f in findings {
            match f.category {
                RefCategory::Protected => by_category.protected += 1,
                RefCategory::SafeToPrune => by_category.safe_to_prune += 1,
                RefCategory::AskUser => by_category.ask_user += 1,
            }
        }
        Self {
            total_refs_scanned: total_scanned,
            findings: findings.len(),
            by_category,
        }
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
        cfg.set_str("user.name", "recovery-test").unwrap();
        cfg.set_str("user.email", "recovery@local").unwrap();
        // Make an initial commit so we have a real oid to play with.
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let sig = Signature::now("r", "r@local").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("a.txt")).unwrap();
        idx.write().unwrap();
        let tree_oid = idx.write_tree().unwrap();
        {
            let tree = repo.find_tree(tree_oid).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();
        }
        repo
    }

    #[test]
    fn clean_repo_has_no_findings() {
        let tmp = TempDir::new().unwrap();
        let _repo = init_repo_with_commit(tmp.path());
        let findings = detect_missing_refs(tmp.path()).unwrap();
        assert!(
            findings.is_empty(),
            "clean repo should have no findings, got {findings:?}"
        );
    }

    #[test]
    fn detects_orphan_stash_ref() {
        let tmp = TempDir::new().unwrap();
        let _repo = init_repo_with_commit(tmp.path());
        // Write a stash ref pointing to a fake oid.
        let fake = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        std::fs::write(tmp.path().join(".git/refs/stash"), format!("{fake}\n")).unwrap();

        let findings = detect_missing_refs(tmp.path()).unwrap();
        assert_eq!(findings.len(), 1, "expected 1 finding, got {findings:?}");
        let f = &findings[0];
        assert_eq!(f.ref_name, "refs/stash");
        assert_eq!(f.target_sha, fake);
        assert_eq!(f.category, RefCategory::SafeToPrune);
    }

    #[test]
    fn detects_orphan_branch_ref() {
        let tmp = TempDir::new().unwrap();
        let _repo = init_repo_with_commit(tmp.path());
        let fake = "cafebabecafebabecafebabecafebabecafebabe";
        // Write a dangling branch ref.
        std::fs::write(
            tmp.path().join(".git/refs/heads/crash-recovery"),
            format!("{fake}\n"),
        )
        .unwrap();

        let findings = detect_missing_refs(tmp.path()).unwrap();
        let crash = findings
            .iter()
            .find(|f| f.ref_name == "refs/heads/crash-recovery")
            .expect("crash ref flagged");
        assert_eq!(crash.target_sha, fake);
        assert_eq!(crash.category, RefCategory::AskUser);
    }

    #[test]
    fn refuses_to_prune_main_without_force() {
        // main gets Protected classification — downstream prune path
        // will refuse regardless of --force (see F3).
        assert_eq!(ref_category("refs/heads/main"), RefCategory::Protected);
        assert_eq!(ref_category("refs/heads/master"), RefCategory::Protected);
        assert_eq!(ref_category("HEAD"), RefCategory::Protected);
    }

    #[test]
    fn ref_category_recognizes_safe_namespaces() {
        assert_eq!(ref_category("refs/stash"), RefCategory::SafeToPrune);
        assert_eq!(ref_category("refs/temp/foo"), RefCategory::SafeToPrune);
        assert_eq!(
            ref_category("refs/original/refs/heads/foo"),
            RefCategory::SafeToPrune
        );
    }

    #[test]
    fn ref_category_ask_user_default() {
        assert_eq!(
            ref_category("refs/heads/feature-branch"),
            RefCategory::AskUser
        );
        assert_eq!(ref_category("refs/tags/v1.0"), RefCategory::AskUser);
        assert_eq!(ref_category("refs/notes/commits"), RefCategory::AskUser);
    }

    #[test]
    fn detection_summary_tallies_categories() {
        let findings = vec![
            PrunableRef {
                ref_name: "refs/stash".to_string(),
                target_sha: "aaa".to_string(),
                reason: "x".to_string(),
                category: RefCategory::SafeToPrune,
            },
            PrunableRef {
                ref_name: "refs/heads/foo".to_string(),
                target_sha: "bbb".to_string(),
                reason: "x".to_string(),
                category: RefCategory::AskUser,
            },
            PrunableRef {
                ref_name: "HEAD".to_string(),
                target_sha: "ccc".to_string(),
                reason: "x".to_string(),
                category: RefCategory::Protected,
            },
        ];
        let s = DetectionSummary::from_findings(42, &findings);
        assert_eq!(s.total_refs_scanned, 42);
        assert_eq!(s.findings, 3);
        assert_eq!(s.by_category.safe_to_prune, 1);
        assert_eq!(s.by_category.ask_user, 1);
        assert_eq!(s.by_category.protected, 1);
    }

    #[test]
    fn mixed_repo_classifies_all_findings() {
        let tmp = TempDir::new().unwrap();
        let _repo = init_repo_with_commit(tmp.path());
        // Stash (safe), crash-recovery heads (ask-user).
        std::fs::write(
            tmp.path().join(".git/refs/stash"),
            "0000000000000000000000000000000000000001\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".git/refs/heads/crash-branch"),
            "0000000000000000000000000000000000000002\n",
        )
        .unwrap();

        let findings = detect_missing_refs(tmp.path()).unwrap();
        assert_eq!(findings.len(), 2, "expected 2 findings: {findings:?}");

        let summary = DetectionSummary::from_findings(findings.len(), &findings);
        assert_eq!(summary.by_category.safe_to_prune, 1);
        assert_eq!(summary.by_category.ask_user, 1);
        assert_eq!(summary.by_category.protected, 0);
    }
}
