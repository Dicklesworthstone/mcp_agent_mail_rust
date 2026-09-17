//! Recovery helpers for repos damaged by the git 2.51.0 index-race.
//!
//! See `docs/GIT_251_FINDINGS.md` for background on the bug and
//! `docs/RECOVERY_RUNBOOK.md` for the operator playbook.
//!
//! # What this module does
//!
//! - [`detect_missing_refs`] (br-8ujfs.6.2 / F2): walk every ref in a
//!   repo and identify ones whose target object is missing from the
//!   object database. These are "orphan" refs left behind when a
//!   writer crashed mid-update.
//! - [`prune_missing_ref`] (F3): revalidate a finding under the Git ref
//!   lock before pruning; callers retain backup and repository-lock ownership.
//! - [`message_reconcile`] (br-8j6cb): restore missing message artifacts
//!   without overwriting conflicting evidence or delivering mail again.
//!
//! # Ref-detection non-goals
//!
//! - Ref detection NEVER touches the working tree. Its operations are pure
//!   ref / ODB introspection; message reconciliation is a separate write API.
//! - We NEVER delete objects. If a ref points to a missing object we
//!   delete the REF; the (missing) object is already gone.
//! - Ref detection NEVER auto-repairs. Detection is strictly read-only; the
//!   caller (Track F's `am doctor fix-orphan-refs` command) decides
//!   when to prune.

pub mod message_reconcile;

use std::path::Path;

use git2::{ErrorCode, ObjectType, Oid, Repository};

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
    //
    // Note: `refs/stash` is a LEAF ref (git stores multiple stashes as a
    // reflog on a single ref tip, not as a namespace), so it's matched
    // exactly rather than via starts_with — otherwise `refs/stashy/foo`
    // would also match, which is wrong.
    if ref_name == "refs/stash" {
        return RefCategory::SafeToPrune;
    }
    const SAFE_PREFIXES: &[&str] = &["refs/temp/", "refs/original/"];
    for prefix in SAFE_PREFIXES {
        if ref_name.starts_with(prefix) {
            return RefCategory::SafeToPrune;
        }
    }

    RefCategory::AskUser
}

/// Result of revalidating an orphan finding at the mutation boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneRefOutcome {
    /// The same direct ref still named a missing object and was removed.
    Pruned,
    /// Another operation already removed the ref.
    AlreadyAbsent,
    /// The ref now has another target or is symbolic. It was not changed.
    Changed,
    /// The original object is now available. Its ref was not changed.
    TargetPresent,
}

/// Prune only the exact, still-missing direct ref described by a finding.
///
/// The caller must obtain its repository coordination lock and successfully
/// write its recovery backup before calling this mutation API. That flock
/// does not serialize ordinary Git clients: a Git ref transaction holds the
/// actual ref lock from revalidation through deletion. Never trust a finding's
/// cached category or delete a ref merely because its name appeared in a scan.
///
/// A changed, symbolic, restored, or already-absent ref is a successful skip,
/// not a deletion. Only `ErrorCode::NotFound` from the ODB proves absence;
/// corruption and I/O errors must not authorize destructive recovery.
///
/// # Errors
///
/// Returns an error for protected refs (even with `force`), an unapproved
/// namespace, malformed OIDs, lock failures, or uncertain repository state.
/// No working-tree files or objects are deleted.
pub fn prune_missing_ref(
    repo_path: &Path,
    finding: &PrunableRef,
    force: bool,
) -> Result<PruneRefOutcome, git2::Error> {
    match ref_category(&finding.ref_name) {
        RefCategory::Protected => {
            return Err(git2::Error::from_str("refusing to prune a protected ref"));
        }
        RefCategory::AskUser if !force => {
            return Err(git2::Error::from_str(
                "refusing to prune an unknown namespace without force",
            ));
        }
        RefCategory::SafeToPrune | RefCategory::AskUser => {}
    }
    let expected = Oid::from_str(&finding.target_sha)?;
    let repo = Repository::open(repo_path)?;
    let mut transaction = repo.transaction()?;
    transaction.lock_ref(&finding.ref_name)?;

    let reference = match repo.find_reference(&finding.ref_name) {
        Ok(reference) => reference,
        Err(error) if error.code() == ErrorCode::NotFound => {
            return Ok(PruneRefOutcome::AlreadyAbsent);
        }
        Err(error) => return Err(error),
    };
    if reference.target() != Some(expected) {
        return Ok(PruneRefOutcome::Changed);
    }
    match repo.odb()?.read_header(expected) {
        Ok(_) => return Ok(PruneRefOutcome::TargetPresent),
        Err(error) if error.code() == ErrorCode::NotFound => {}
        Err(error) => return Err(error),
    }

    transaction.remove(&finding.ref_name)?;
    transaction.commit()?;
    Ok(PruneRefOutcome::Pruned)
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
            } else if let Ok(Some(sym)) = reference.symbolic_target() {
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

        if odb.exists(oid) {
            tracing::trace!(
                target: "mcp_agent_mail::storage::recovery",
                ref = %name,
                oid = %oid,
                via = peel_reason,
                "recovery_ref_intact"
            );
        } else {
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
    fn ref_category_refs_stash_is_leaf_not_prefix() {
        // `refs/stash` is a SINGLE ref (git stores multiple stashes as a
        // reflog on that tip). Anything that merely starts with "refs/stash"
        // — say `refs/stashy/*` from a user's ad-hoc naming — must NOT
        // inherit the SafeToPrune category.
        assert_eq!(ref_category("refs/stash"), RefCategory::SafeToPrune);
        assert_eq!(ref_category("refs/stashy/foo"), RefCategory::AskUser);
        assert_eq!(ref_category("refs/stash-backup"), RefCategory::AskUser);
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

    fn write_orphan(repo: &Repository, name: &str, oid: Oid) -> PrunableRef {
        let path = repo.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("{oid}\n")).unwrap();
        PrunableRef {
            ref_name: name.to_string(),
            target_sha: oid.to_string(),
            reason: "test missing object".to_string(),
            category: ref_category(name),
        }
    }

    fn missing_oid() -> Oid {
        Oid::from_str("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap()
    }

    #[test]
    fn guarded_prune_removes_only_the_same_missing_ref() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let head = repo.head().unwrap().target().unwrap();
        let finding = write_orphan(&repo, "refs/temp/orphan", missing_oid());
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::Pruned
        );
        assert!(repo.find_reference(&finding.ref_name).is_err());
        assert_eq!(repo.head().unwrap().target(), Some(head));
        assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"hello\n");
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::AlreadyAbsent
        );
    }

    #[test]
    fn guarded_prune_preserves_a_ref_repaired_after_detection() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/repaired", missing_oid());
        let head = repo.head().unwrap().target().unwrap();
        repo.reference(&finding.ref_name, head, true, "concurrent repair")
            .unwrap();
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::Changed
        );
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(head)
        );
    }

    #[test]
    fn guarded_prune_preserves_a_different_missing_target() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/changed", missing_oid());
        let changed = Oid::from_str("cafebabecafebabecafebabecafebabecafebabe").unwrap();
        write_orphan(&repo, &finding.ref_name, changed);
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::Changed
        );
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(changed)
        );
    }

    #[test]
    fn guarded_prune_preserves_an_object_restored_under_the_same_oid() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let bytes = b"object restored after detection";
        let oid = Oid::hash_object(ObjectType::Blob, bytes).unwrap();
        let finding = write_orphan(&repo, "refs/temp/restored", oid);
        assert_eq!(
            repo.odb().unwrap().write(ObjectType::Blob, bytes).unwrap(),
            oid
        );
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::TargetPresent
        );
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(oid)
        );
    }

    #[test]
    fn guarded_prune_preserves_a_ref_changed_to_symbolic() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/symbolic", missing_oid());
        std::fs::write(
            repo.path().join(&finding.ref_name),
            "ref: refs/heads/not-created\n",
        )
        .unwrap();
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::Changed
        );
        assert_eq!(
            repo.find_reference(&finding.ref_name)
                .unwrap()
                .symbolic_target()
                .unwrap(),
            Some("refs/heads/not-created")
        );
    }

    #[test]
    fn guarded_prune_reclassifies_names_instead_of_trusting_findings() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        for name in ["refs/heads/main", "refs/remotes/origin/main"] {
            let mut finding = write_orphan(&repo, name, missing_oid());
            finding.category = RefCategory::SafeToPrune;
            assert!(prune_missing_ref(tmp.path(), &finding, true).is_err());
            assert!(repo.find_reference(name).is_ok());
        }
        let mut finding = write_orphan(&repo, "refs/heads/recovery-topic", missing_oid());
        finding.category = RefCategory::SafeToPrune;
        assert!(prune_missing_ref(tmp.path(), &finding, false).is_err());
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, true).unwrap(),
            PruneRefOutcome::Pruned
        );
    }

    #[test]
    fn guarded_prune_refuses_a_busy_git_ref_lock() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/locked", missing_oid());
        let mut writer = repo.transaction().unwrap();
        writer.lock_ref(&finding.ref_name).unwrap();
        assert!(prune_missing_ref(tmp.path(), &finding, false).is_err());
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(missing_oid())
        );
    }

    #[test]
    fn guarded_prune_refuses_a_corrupt_object_instead_of_treating_it_as_missing() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/corrupt", missing_oid());
        let oid = finding.target_sha.as_str();
        let dir = repo.path().join("objects").join(&oid[..2]);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&oid[2..]), b"not a zlib object").unwrap();
        assert!(prune_missing_ref(tmp.path(), &finding, false).is_err());
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(missing_oid())
        );
    }
}
