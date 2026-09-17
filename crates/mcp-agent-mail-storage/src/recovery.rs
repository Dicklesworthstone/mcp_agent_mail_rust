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

    /// The original direct target OID, not a peeled tag-chain target.
    /// Locked pruning compares this exact identity before removing the ref.
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

/// Detect direct refs whose target objects are missing from the repo's ODB.
///
/// This uses libgit2 rather than invoking `git fsck`. A missing direct target
/// is a finding; an unreadable reference/object or an unresolved symbolic/tag
/// chain is an error. Callers must not mistake an incomplete scan for a clean
/// repository or use its partial findings to authorize destructive recovery.
///
/// # Arguments
///
/// - `repo_path`: path to the repo (normal, bare, or worktree).
///
/// # Returns
///
/// One [`PrunableRef`] per confirmed missing direct target. Each finding keeps
/// the original ref OID for mutation-time revalidation. An empty result means
/// that all enumerated refs resolved, their object headers were readable, and
/// any annotated tags peeled successfully. This is not a complete audit of
/// commit/tree reachability, object contents, or pseudorefs such as HEAD.
///
/// # Errors
///
/// Returns an error if the repo, ref iterator, ref name, symbolic resolution,
/// object header, or tag chain cannot be read reliably. Only an ODB
/// `ErrorCode::NotFound` for a direct ref produces a prunable finding.
pub fn detect_missing_refs(repo_path: &Path) -> Result<Vec<PrunableRef>, git2::Error> {
    let repo = Repository::open(repo_path)?;
    let odb = repo.odb()?;
    let mut out = Vec::new();

    for reference in repo.references()? {
        // Never silently drop an iterator error or manufacture a ref name.
        let reference = reference?;
        let name = reference.name()?.to_string();
        let resolved = reference.resolve().map_err(|error| {
            git2::Error::from_str(&format!("cannot resolve reference {name}: {error}"))
        })?;
        let oid = resolved.target().ok_or_else(|| {
            git2::Error::from_str(&format!("reference {name} has no resolved direct target"))
        })?;

        match odb.read_header(oid) {
            Ok((_, kind)) => {
                // A readable tag object is not sufficient: its target chain
                // can still be missing or corrupt. Do not fall back to the
                // tag's own OID after a failed peel and call the ref healthy.
                if kind == ObjectType::Tag {
                    resolved.peel(ObjectType::Any).map_err(|error| {
                        git2::Error::from_str(&format!(
                            "cannot validate tag chain for {name}: {error}"
                        ))
                    })?;
                }
                tracing::trace!(
                    target: "mcp_agent_mail::storage::recovery",
                    ref = %name,
                    oid = %oid,
                    "recovery_ref_target_readable"
                );
            }
            Err(error)
                if error.code() == ErrorCode::NotFound && reference.target().is_some() =>
            {
                let category = ref_category(&name);
                out.push(PrunableRef {
                    ref_name: name.clone(),
                    target_sha: oid.to_string(),
                    reason: format!("object {oid} missing from ODB (via direct-target)"),
                    category,
                });
                tracing::info!(
                    target: "mcp_agent_mail::storage::recovery",
                    ref = %name,
                    oid = %oid,
                    category = ?category,
                    "recovery_ref_missing_object"
                );
            }
            Err(error) => {
                // Includes symbolic aliases resolving to missing objects.
                // An alias is not authority to prune its target or itself.
                return Err(git2::Error::from_str(&format!(
                    "cannot validate target {oid} for reference {name}: {error}"
                )));
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
    fn ref_category_refs_stash_is_leaf_not_prefix() {
        // `refs/stash` is a SINGLE ref (git stores multiple stashes as a
        // reflog on a single ref tip, not as a namespace), so it's matched
        // exactly rather than via starts_with — otherwise `refs/stashy/*`
        // would also match, which is wrong.
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
        assert_eq!(repo.find_reference(&finding.ref_name).unwrap().target(), Some(head));
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
        assert_eq!(repo.find_reference(&finding.ref_name).unwrap().target(), Some(changed));
    }

    #[test]
    fn guarded_prune_preserves_an_object_restored_under_the_same_oid() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let bytes = b"object restored after detection";
        let oid = Oid::hash_object(ObjectType::Blob, bytes).unwrap();
        let finding = write_orphan(&repo, "refs/temp/restored", oid);
        assert_eq!(repo.odb().unwrap().write(ObjectType::Blob, bytes).unwrap(), oid);
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::TargetPresent
        );
        assert_eq!(repo.find_reference(&finding.ref_name).unwrap().target(), Some(oid));
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

    #[test]
    fn detection_rejects_corrupt_object_headers_without_changing_evidence() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/corrupt-scan", missing_oid());
        let oid = finding.target_sha.as_str();
        let dir = repo.path().join("objects").join(&oid[..2]);
        std::fs::create_dir_all(&dir).unwrap();
        let object_path = dir.join(&oid[2..]);
        std::fs::write(&object_path, b"not a zlib object").unwrap();

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert_eq!(std::fs::read(&object_path).unwrap(), b"not a zlib object");
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(missing_oid())
        );
    }

    #[test]
    fn detection_rejects_a_dangling_symbolic_ref() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let path = repo.path().join("refs/heads/alias");
        let bytes = b"ref: refs/heads/nonexistent\n";
        std::fs::write(&path, bytes).unwrap();

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn detection_rejects_a_symbolic_ref_cycle() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        std::fs::write(
            repo.path().join("refs/heads/alias-a"),
            b"ref: refs/heads/alias-b\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("refs/heads/alias-b"),
            b"ref: refs/heads/alias-a\n",
        )
        .unwrap();

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert!(repo.find_reference("refs/heads/alias-a").is_ok());
        assert!(repo.find_reference("refs/heads/alias-b").is_ok());
    }

    #[test]
    fn detection_does_not_turn_an_alias_into_prune_authority() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/direct", missing_oid());
        std::fs::write(
            repo.path().join("refs/temp/alias"),
            b"ref: refs/temp/direct\n",
        )
        .unwrap();

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(missing_oid())
        );
    }

    #[test]
    fn detection_rejects_an_existing_tag_with_a_missing_target() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let text = format!(
            "object {}\ntype commit\ntag broken\ntagger Test <test@local> 1700000000 +0000\n\nbroken target\n",
            missing_oid()
        );
        let tag_oid = repo
            .odb()
            .unwrap()
            .write(ObjectType::Tag, text.as_bytes())
            .unwrap();
        let finding = write_orphan(&repo, "refs/temp/broken-tag", tag_oid);

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(tag_oid)
        );
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::TargetPresent
        );
    }

    #[test]
    fn detection_accepts_valid_symbolic_refs_and_tag_chains() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        let signature = Signature::now("tag-test", "tag@local").unwrap();
        let tag_oid = repo
            .tag("healthy", commit.as_object(), &signature, "healthy", false)
            .unwrap();
        let tag = repo.find_object(tag_oid, Some(ObjectType::Tag)).unwrap();
        repo.tag("nested", &tag, &signature, "nested", false)
            .unwrap();
        std::fs::write(
            repo.path().join("refs/heads/alias"),
            format!("ref: {}\n", head.name().unwrap()),
        )
        .unwrap();

        assert!(detect_missing_refs(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn detection_accepts_an_empty_unborn_repository() {
        let tmp = TempDir::new().unwrap();
        let _repo = Repository::init(tmp.path()).unwrap();
        assert!(detect_missing_refs(tmp.path()).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn detection_refuses_non_utf8_names_instead_of_inventing_a_prunable_name() {
        use std::os::unix::ffi::OsStringExt;

        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let name = std::ffi::OsString::from_vec(b"invalid-\xff".to_vec());
        let path = repo.path().join("refs/heads").join(name);
        let before = format!("{}\n", missing_oid());
        std::fs::write(&path, &before).unwrap();

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), before);
    }
}
