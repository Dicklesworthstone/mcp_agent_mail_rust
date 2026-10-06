//! `fm-archive-state-files-missing-head-or-broken-git-shape` —
//! P0 detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! The shared `<storage_root>/.git/` archive and any legacy
//! `<storage_root>/projects/<slug>/.git/` archives must
//! have a valid `HEAD` file that either:
//! - Points to a SHA directly (detached HEAD), OR
//! - Contains `ref: refs/heads/<branch>` AND the target ref
//!   resolves via a loose ref file OR `packed-refs` (or is the
//!   unborn branch of a newly initialized repository).
//!
//! If HEAD is missing, empty, unreadable, a symlink, or points
//! at a dangling ref, the project's git archive can't be opened
//! by `git ls-files`, `git log`, etc. — `am`'s archive replay
//! and reservation tooling break silently.
//!
//! ## Detection
//!
//! For the shared storage root and each legacy project archive:
//! 1. If `.git/` doesn't exist, skip (sibling FM territory).
//! 2. lstat `.git/HEAD`:
//!    - Symlink → `HeadIsSymlink` (rejected for security).
//!    - Missing → `HeadMissing`.
//!    - Unreadable → `HeadUnreadable`.
//!    - Empty → `HeadEmpty`.
//! 3. If HEAD starts with `ref: `, resolve the target:
//!    - Loose ref file at `.git/<target>` exists, OR
//!    - `.git/packed-refs` contains a line ending in `<target>`.
//!    - Neither → `HeadDanglingRef`.
//!
//! ## Fix
//!
//! **Detect-only.** Preserve the archive and database before investigating
//! Git refs, reflogs and backups. Restoring HEAD requires an authoritative
//! branch or commit; never guess it. `am doctor reconstruct` rebuilds the
//! database FROM the archive and cannot repair missing Git history.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-archive-state-files-missing-head-or-broken-git-shape";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "archive_state_files";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrokenShape {
    /// `.git/HEAD` does not exist.
    HeadMissing { repo_path: PathBuf },
    /// `.git/HEAD` exists but cannot be read (permission denied,
    /// I/O error).
    HeadUnreadable { repo_path: PathBuf },
    /// `.git/HEAD` is a symlink (rejected for security —
    /// archive HEAD files must be regular files).
    HeadIsSymlink { repo_path: PathBuf },
    /// `.git/HEAD` is a regular file but contains nothing
    /// (zero-length or only whitespace).
    HeadEmpty { repo_path: PathBuf },
    /// `.git/HEAD` contains `ref: <target>` but the target ref
    /// is neither a loose ref file nor present in
    /// `packed-refs`.
    HeadDanglingRef {
        repo_path: PathBuf,
        target_ref: String,
    },
    /// `.git/HEAD` is structurally valid and its ref resolves, but the commit
    /// object it names is missing/corrupt in the object store ("fatal: bad
    /// object HEAD"). Left behind by an interrupted `gc`/repack after a hard
    /// reboot (the ts2 incident). The commit coalescer self-heals this by
    /// re-rooting onto the working tree (br-bvq1x.9.7); this is the proactive
    /// detection surface.
    HeadPointsToMissingObject { repo_path: PathBuf },
}

#[cfg(test)]
impl BrokenShape {
    fn repo_path(&self) -> &PathBuf {
        match self {
            BrokenShape::HeadMissing { repo_path }
            | BrokenShape::HeadUnreadable { repo_path }
            | BrokenShape::HeadIsSymlink { repo_path }
            | BrokenShape::HeadEmpty { repo_path }
            | BrokenShape::HeadDanglingRef { repo_path, .. }
            | BrokenShape::HeadPointsToMissingObject { repo_path } => repo_path,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingHeadOrBrokenGitShapeFinding {
    pub broken: Vec<BrokenShape>,
}

impl MissingHeadOrBrokenGitShapeFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} Git archive(s) have broken .git shape (missing / empty / symlinked / dangling HEAD, or HEAD pointing at a missing object)",
            self.broken.len(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "broken": self.broken,
                "count": self.broken.len(),
                "manual_remediation": {
                    "steps": [
                        "Run `am doctor locks --json` and use the reported supervisor protocol to drain the mailbox before any repair.",
                        "Preserve the entire archive, including .git, and the SQLite database with its sidecars before changing either authority.",
                        "Inspect refs, reflogs and backups to identify the authoritative branch or commit. Restore a known HEAD or repository backup reversibly; do not guess HEAD or reinitialize over existing history.",
                        "After recovery, re-run `am doctor fix --only fm-archive-state-files-missing-head-or-broken-git-shape --list --json` and verify that new mail reaches its canonical Markdown archive.",
                    ],
                    "warning": "A symlinked HEAD is a SECURITY signal (attacker may be aliasing HEAD at an arbitrary file). Investigate it before repair. `am doctor reconstruct` rebuilds SQLite FROM the archive; do not use an empty or incomplete archive to recover a healthy database.",
                    "note": "Auto-fix via Op::WriteFile is intentionally not implemented — repairing a broken git shape needs operator judgment about which branch HEAD should point at.",
                    "head_points_to_missing_object": "The commit coalescer can recover this variant from an intact working tree (br-bvq1x.9.7). Preserve available history and inspect the refs and object store before manual recovery.",
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Detector. Read-only: file-level HEAD/ref inspection, plus (when the repo
/// opens) a read-only git2 probe that the HEAD tip object actually loads.
///
/// Scan the shared root even when no project directories exist yet. Legacy
/// project repositories remain supported; entries without `.git/` are skipped.
pub fn detect(
    project_dirs: &[PathBuf],
    storage_root: Option<&Path>,
) -> Vec<MissingHeadOrBrokenGitShapeFinding> {
    let mut broken: Vec<BrokenShape> = Vec::new();
    for repo_path in storage_root.into_iter().chain(
        project_dirs
            .iter()
            .map(PathBuf::as_path)
            .filter(|path| Some(*path) != storage_root),
    ) {
        let git_dir = repo_path.join(".git");
        if !git_dir.is_dir() {
            continue;
        }
        if let Some(shape) = inspect_head(&git_dir, repo_path) {
            broken.push(shape);
        }
    }
    if broken.is_empty() {
        return Vec::new();
    }
    vec![MissingHeadOrBrokenGitShapeFinding { broken }]
}

fn inspect_head(git_dir: &std::path::Path, repo_path: &std::path::Path) -> Option<BrokenShape> {
    let head_path = git_dir.join("HEAD");

    let lmeta = match fs::symlink_metadata(&head_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(BrokenShape::HeadMissing {
                repo_path: repo_path.to_path_buf(),
            });
        }
        Err(_) => {
            return Some(BrokenShape::HeadUnreadable {
                repo_path: repo_path.to_path_buf(),
            });
        }
    };

    if lmeta.file_type().is_symlink() {
        return Some(BrokenShape::HeadIsSymlink {
            repo_path: repo_path.to_path_buf(),
        });
    }

    let head_bytes = match fs::read(&head_path) {
        Ok(b) => b,
        Err(_) => {
            return Some(BrokenShape::HeadUnreadable {
                repo_path: repo_path.to_path_buf(),
            });
        }
    };

    let s = std::str::from_utf8(&head_bytes).unwrap_or("").trim();
    if s.is_empty() {
        return Some(BrokenShape::HeadEmpty {
            repo_path: repo_path.to_path_buf(),
        });
    }

    if let Some(target_ref) = s.strip_prefix("ref: ") {
        let target_ref = target_ref.trim();
        let loose = git_dir.join(target_ref);
        let packed = git_dir.join("packed-refs");
        let resolved_loose = loose.is_file();
        let resolved_packed = packed.is_file()
            && fs::read_to_string(&packed)
                .map(|p| {
                    p.lines()
                        .any(|l| !l.starts_with('#') && l.ends_with(target_ref))
                })
                .unwrap_or(false);
        if !resolved_loose && !resolved_packed {
            // `git init` creates HEAD before the first branch ref. A real
            // empty repository is valid; a dangling HEAD alongside existing
            // refs still needs operator investigation.
            if let Ok(repo) = git2::Repository::open(repo_path)
                && repo
                    .head()
                    .is_err_and(|error| error.code() == git2::ErrorCode::UnbornBranch)
                && repo
                    .references()
                    .is_ok_and(|mut refs| refs.next().is_none())
            {
                return None;
            }
            return Some(BrokenShape::HeadDanglingRef {
                repo_path: repo_path.to_path_buf(),
                target_ref: target_ref.to_string(),
            });
        }
    }

    // The HEAD file is structurally valid (and any `ref:` target resolves to a
    // ref). Confirm the commit object that ref/SHA names is actually loadable —
    // an interrupted gc/repack leaves HEAD pointing at a missing object
    // ("fatal: bad object HEAD"; the ts2 incident). Gated on a successful git2
    // open so the cheap file-level path is preserved and non-repos / minimal
    // fixtures (no real object store) are skipped rather than mis-flagged.
    if let Ok(repo) = git2::Repository::open(repo_path)
        && let Ok(oid) = repo.refname_to_id("HEAD")
        && repo.find_object(oid, None).is_err()
    {
        return Some(BrokenShape::HeadPointsToMissingObject {
            repo_path: repo_path.to_path_buf(),
        });
    }
    None
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &MissingHeadOrBrokenGitShapeFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a project repo with a valid HEAD pointing at a
    /// loose ref that exists.
    fn make_healthy_repo(td: &TempDir, slug: &str) -> PathBuf {
        let repo = td.path().join(slug);
        let git_dir = repo.join(".git");
        let refs_dir = git_dir.join("refs").join("heads");
        fs::create_dir_all(&refs_dir).unwrap();
        fs::write(refs_dir.join("main"), "deadbeef\n").unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        repo
    }

    /// **NEGATIVE TEST FIRST** (pass-35V lesson): a healthy repo
    /// with valid HEAD → no finding.
    #[test]
    fn detector_skips_healthy_repos() {
        let td = TempDir::new().unwrap();
        let repo = make_healthy_repo(&td, "a");
        let findings = detect(&[repo], None);
        assert!(
            findings.is_empty(),
            "healthy HEAD must not produce a finding"
        );
    }

    /// **NEGATIVE**: project dir without `.git/` → skipped
    /// (sibling FM territory).
    #[test]
    fn detector_skips_project_without_git_dir() {
        let td = TempDir::new().unwrap();
        let repo = td.path().join("a");
        fs::create_dir_all(&repo).unwrap();
        let findings = detect(&[repo], None);
        assert!(findings.is_empty());
    }

    /// **NEGATIVE**: detached HEAD (raw SHA, no `ref:` prefix)
    /// is valid.
    #[test]
    fn detector_accepts_detached_head() {
        let td = TempDir::new().unwrap();
        let repo = td.path().join("a");
        let git_dir = repo.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("HEAD"),
            "deadbeefcafebabe1234567890abcdef00000000\n",
        )
        .unwrap();
        let findings = detect(&[repo], None);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_empty_input() {
        assert!(detect(&[], None).is_empty());
    }

    #[test]
    fn detector_checks_shared_root_before_any_project_exists() {
        let td = TempDir::new().unwrap();
        fs::create_dir(td.path().join(".git")).unwrap();
        let findings = detect(&[], Some(td.path()));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].broken,
            vec![BrokenShape::HeadMissing {
                repo_path: td.path().to_path_buf(),
            }]
        );
        assert!(!td.path().join("projects").exists());
        assert!(!td.path().join(".git/HEAD").exists());
    }

    #[test]
    fn detector_accepts_real_unborn_shared_root() {
        let td = TempDir::new().unwrap();
        git2::Repository::init(td.path()).unwrap();
        assert!(detect(&[], Some(td.path())).is_empty());
    }

    #[test]
    fn detector_checks_shared_and_legacy_roots_once_each() {
        let td = TempDir::new().unwrap();
        let legacy = td.path().join("projects/legacy");
        fs::create_dir_all(legacy.join(".git")).unwrap();
        fs::create_dir(td.path().join(".git")).unwrap();
        let findings = detect(&[td.path().to_path_buf(), legacy.clone()], Some(td.path()));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].broken.len(), 2);
        assert_eq!(findings[0].broken[0].repo_path(), td.path());
        assert_eq!(findings[0].broken[1].repo_path(), &legacy);
    }

    /// br-bvq1x.9.8 (ts2): a real repo whose HEAD tip object is missing
    /// ("fatal: bad object HEAD" after an interrupted gc/repack) is flagged.
    /// The negative cases above (fake `.git` with no object store) confirm the
    /// git2 probe is gated on a successful repo open and never mis-flags them.
    #[test]
    fn detector_flags_head_pointing_at_missing_object() {
        let td = TempDir::new().unwrap();
        let repo_path = td.path().join("proj");
        let repo = git2::Repository::init(&repo_path).expect("init repo");
        {
            let sig = git2::Signature::now("t", "t@example.com").expect("sig");
            let tree_oid = {
                let mut index = repo.index().expect("index");
                index.write_tree().expect("write_tree")
            };
            let tree = repo.find_tree(tree_oid).expect("tree");
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .expect("commit");
        }
        // Healthy now: the tip object loads, so no finding.
        assert!(detect(std::slice::from_ref(&repo_path), None).is_empty());

        // Corrupt the branch tip to a SHA that is not in the object store.
        let refname = repo.head().unwrap().name().expect("refname").to_string();
        drop(repo);
        fs::write(
            repo_path.join(".git").join(&refname),
            format!("{}\n", "de".repeat(20)),
        )
        .unwrap();

        let findings = detect(std::slice::from_ref(&repo_path), None);
        assert_eq!(findings.len(), 1, "missing-tip-object HEAD must be flagged");
        assert!(
            matches!(
                findings[0].broken[0],
                BrokenShape::HeadPointsToMissingObject { .. }
            ),
            "unexpected shape: {:?}",
            findings[0].broken
        );
    }

    #[test]
    fn detector_flags_missing_head() {
        let td = TempDir::new().unwrap();
        let repo = td.path().join("a");
        let git_dir = repo.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let findings = detect(std::slice::from_ref(&repo), None);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].broken.len(), 1);
        assert!(matches!(
            &findings[0].broken[0],
            BrokenShape::HeadMissing { repo_path } if repo_path == &repo
        ));
    }

    #[test]
    fn detector_flags_empty_head() {
        let td = TempDir::new().unwrap();
        let repo = td.path().join("a");
        let git_dir = repo.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "  \n").unwrap();
        let findings = detect(&[repo], None);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            &findings[0].broken[0],
            BrokenShape::HeadEmpty { .. }
        ));
    }

    #[test]
    fn detector_flags_symlinked_head() {
        let td = TempDir::new().unwrap();
        let repo = td.path().join("a");
        let git_dir = repo.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let target = td.path().join("bogus_target");
        fs::write(&target, b"deadbeef").unwrap();
        std::os::unix::fs::symlink(&target, git_dir.join("HEAD")).unwrap();
        let findings = detect(&[repo], None);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            &findings[0].broken[0],
            BrokenShape::HeadIsSymlink { .. }
        ));
    }

    #[test]
    fn detector_flags_dangling_ref() {
        let td = TempDir::new().unwrap();
        let repo = td.path().join("a");
        let git_dir = repo.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        // HEAD points at a ref that doesn't exist (loose or packed).
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/missing\n").unwrap();
        let findings = detect(&[repo], None);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            &findings[0].broken[0],
            BrokenShape::HeadDanglingRef { target_ref, .. } if target_ref == "refs/heads/missing"
        ));
    }

    #[test]
    fn detector_resolves_via_packed_refs() {
        let td = TempDir::new().unwrap();
        let repo = td.path().join("a");
        let git_dir = repo.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        // No loose ref, but packed-refs has it.
        fs::write(
            git_dir.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted \ndeadbeef refs/heads/main\n",
        )
        .unwrap();
        let findings = detect(&[repo], None);
        assert!(
            findings.is_empty(),
            "HEAD resolvable via packed-refs must not flag"
        );
    }

    #[test]
    fn detector_aggregates_multiple_broken_repos_into_one_finding() {
        let td = TempDir::new().unwrap();
        let repo_a = td.path().join("a");
        let git_a = repo_a.join(".git");
        fs::create_dir_all(&git_a).unwrap();
        // a: missing HEAD
        let repo_b = td.path().join("b");
        let git_b = repo_b.join(".git");
        fs::create_dir_all(&git_b).unwrap();
        fs::write(git_b.join("HEAD"), "").unwrap();
        // b: empty HEAD
        let findings = detect(&[repo_a, repo_b], None);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].broken.len(), 2);
    }

    #[test]
    fn finding_serializes_with_kind_and_warning() {
        let f = MissingHeadOrBrokenGitShapeFinding {
            broken: vec![BrokenShape::HeadMissing {
                repo_path: "/var/data/projects/foo".into(),
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("head_missing"));
        assert!(s.contains("\"count\":1"));
        assert!(s.contains("\"auto_fixable\":false"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), "test_run").unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = crate::doctor::mutate::MutateContext {
            run_id: "test_run".into(),
            run_dir,
            capabilities: crate::doctor::mutate::Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: std::sync::Mutex::new(actions),
            fixer_id: FM_ID.into(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: std::time::Instant::now(),
            extra_locks: Vec::new(),
        };
        let finding = MissingHeadOrBrokenGitShapeFinding { broken: vec![] };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    /// Bonus: verify that repo_path() accessor returns the right
    /// path for each BrokenShape variant.
    #[test]
    fn broken_shape_repo_path_accessor() {
        let p: PathBuf = "/x".into();
        let cases = [
            BrokenShape::HeadMissing {
                repo_path: p.clone(),
            },
            BrokenShape::HeadUnreadable {
                repo_path: p.clone(),
            },
            BrokenShape::HeadIsSymlink {
                repo_path: p.clone(),
            },
            BrokenShape::HeadEmpty {
                repo_path: p.clone(),
            },
            BrokenShape::HeadDanglingRef {
                repo_path: p.clone(),
                target_ref: "refs/heads/x".to_string(),
            },
        ];
        for c in &cases {
            assert_eq!(c.repo_path(), &p);
        }
    }
}
