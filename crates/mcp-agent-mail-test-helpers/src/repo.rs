//! `repo`: factory for tempdir git repos in the shapes tests need.
//!
//! All factories return a [`RepoFixture`] that owns both the tempdir and
//! the path into it — so the caller just keeps the `RepoFixture` alive
//! and cleanup happens in `Drop`.
//!
//! Each factory uses libgit2 directly (`git2` crate) so it works without
//! a system git binary and doesn't exercise the very CLI path we're
//! trying to avoid.

use std::fs;
use std::path::{Path, PathBuf};

use git2::{Repository, Signature};
use tempfile::TempDir;

/// A tempdir-backed git repo in a specific shape.
///
/// Keeping `_guard` private ensures the TempDir outlives the PathBuf.
pub struct RepoFixture {
    pub path: PathBuf,
    _guard: TempDir,
}

impl RepoFixture {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn new_tempdir() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let p = tmp.path().join("repo");
    fs::create_dir_all(&p).unwrap();
    (tmp, p)
}

fn signature() -> Signature<'static> {
    Signature::now("test-helpers", "test@local").expect("sig")
}

/// Builder for richer repo states composed from primitives.
pub struct RepoBuilder {
    guard: TempDir,
    path: PathBuf,
    repo: Repository,
    last_commit: Option<git2::Oid>,
}

impl RepoBuilder {
    /// Start a fresh empty repo.
    #[must_use]
    pub fn new() -> Self {
        let (guard, path) = new_tempdir();
        let repo = Repository::init(&path).expect("Repository::init failed in test helpers");
        // Config required for commits.
        let mut cfg = repo.config().expect("config");
        cfg.set_str("user.name", "test-helpers").unwrap();
        cfg.set_str("user.email", "test@local").unwrap();
        Self {
            guard,
            path,
            repo,
            last_commit: None,
        }
    }

    /// Create a new branch called `main` with a single commit containing
    /// a README so HEAD is born.
    pub fn commit_initial(mut self, content: &str) -> Self {
        let readme = self.path.join("README.md");
        fs::write(&readme, content).unwrap();
        let oid = {
            let mut idx = self.repo.index().unwrap();
            idx.add_path(Path::new("README.md")).unwrap();
            idx.write().unwrap();
            let tree_oid = idx.write_tree().unwrap();
            let tree = self.repo.find_tree(tree_oid).unwrap();
            let sig = signature();
            self.repo
                .commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
                .unwrap()
        };
        self.last_commit = Some(oid);
        self
    }

    /// Add a named file and commit it on top of current HEAD.
    pub fn add_file_and_commit(mut self, name: &str, content: &str) -> Self {
        let p = self.path.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, content).unwrap();
        let oid = {
            let mut idx = self.repo.index().unwrap();
            idx.add_path(Path::new(name)).unwrap();
            idx.write().unwrap();
            let tree_oid = idx.write_tree().unwrap();
            let tree = self.repo.find_tree(tree_oid).unwrap();
            let sig = signature();
            let parents: Vec<git2::Commit<'_>> = self
                .last_commit
                .map(|o| self.repo.find_commit(o).unwrap())
                .into_iter()
                .collect();
            let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
            self.repo
                .commit(
                    Some("HEAD"),
                    &sig,
                    &sig,
                    &format!("add {name}"),
                    &tree,
                    &parent_refs,
                )
                .unwrap()
        };
        self.last_commit = Some(oid);
        self
    }

    /// Finish and return the fixture.
    #[must_use]
    pub fn build(self) -> RepoFixture {
        // Drop the repo handle so the caller can open a fresh one.
        let RepoBuilder { guard, path, .. } = self;
        RepoFixture {
            path,
            _guard: guard,
        }
    }
}

impl Default for RepoBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Empty repo: initialized, no commits, HEAD is unborn.
#[must_use]
pub fn empty() -> RepoFixture {
    let (guard, path) = new_tempdir();
    let _ = Repository::init(&path).expect("init empty repo");
    RepoFixture {
        path,
        _guard: guard,
    }
}

/// Single-commit repo with one file ("README.md" / "hello").
#[must_use]
pub fn single_commit() -> RepoFixture {
    RepoBuilder::new().commit_initial("hello\n").build()
}

/// Repo with N commits, each adding `file_<i>.txt`.
#[must_use]
pub fn with_commits(n: usize) -> RepoFixture {
    let mut b = RepoBuilder::new().commit_initial("seed\n");
    for i in 0..n.saturating_sub(1) {
        b = b.add_file_and_commit(&format!("file_{i:04}.txt"), "x\n");
    }
    b.build()
}

/// Repo whose HEAD is unborn but which has a `.git/` directory.
#[must_use]
pub fn unborn_head() -> RepoFixture {
    empty()
}

/// Repo with a dangling `refs/heads/crash-recovery` pointing at a SHA
/// that doesn't exist in the object database. Emulates the damage
/// pattern seen on 2.51.0 boxes after a segfault interrupts a stash
/// or branch write.
///
/// Used by Track F tests (F2/F3/F6) as the ground-truth damage fixture.
#[must_use]
pub fn with_dangling_branch() -> RepoFixture {
    let fix = single_commit();
    let ref_path = fix
        .path
        .join(".git")
        .join("refs")
        .join("heads")
        .join("crash-recovery");
    let fake_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    if let Some(parent) = ref_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&ref_path, format!("{fake_sha}\n")).unwrap();
    fix
}

/// Repo with a stash-like ref whose target object is missing from the
/// ODB. We emulate this by committing a file, creating a stash ref that
/// points at an orphan commit, then deleting its blob storage.
///
/// Not a true `git stash push` — that's slow and flaky in tempdirs.
/// The important invariant for tests: a ref exists whose peeled oid
/// cannot be found via `Repository::odb().exists()`.
#[must_use]
pub fn with_orphan_stash_ref() -> RepoFixture {
    let fix = single_commit();
    let fake_sha = "cafebabecafebabecafebabecafebabecafebabe";
    let ref_path = fix.path.join(".git").join("refs").join("stash");
    fs::write(&ref_path, format!("{fake_sha}\n")).unwrap();
    fix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_repo_has_unborn_head() {
        let fix = empty();
        let repo = Repository::open(&fix.path).unwrap();
        assert!(repo.head().is_err(), "empty repo should have unborn HEAD");
    }

    #[test]
    fn single_commit_repo_has_head() {
        let fix = single_commit();
        let repo = Repository::open(&fix.path).unwrap();
        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        assert_eq!(commit.message().unwrap_or(""), "initial");
    }

    #[test]
    fn with_commits_produces_expected_count() {
        let fix = with_commits(5);
        let repo = Repository::open(&fix.path).unwrap();
        let mut revwalk = repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        let count = revwalk.count();
        assert_eq!(count, 5, "expected 5 commits");
    }

    #[test]
    fn dangling_branch_points_at_missing_object() {
        let fix = with_dangling_branch();
        let repo = Repository::open(&fix.path).unwrap();
        let r = repo
            .find_reference("refs/heads/crash-recovery")
            .expect("ref exists");
        let target = r.target().expect("direct target");
        assert!(
            !repo.odb().unwrap().exists(target),
            "dangling ref's target should NOT exist in the ODB"
        );
    }

    #[test]
    fn orphan_stash_ref_has_missing_target() {
        let fix = with_orphan_stash_ref();
        let repo = Repository::open(&fix.path).unwrap();
        let r = repo.find_reference("refs/stash").expect("ref exists");
        let target = r.target().expect("direct target");
        assert!(
            !repo.odb().unwrap().exists(target),
            "orphan stash ref's target should NOT exist in the ODB"
        );
    }
}
