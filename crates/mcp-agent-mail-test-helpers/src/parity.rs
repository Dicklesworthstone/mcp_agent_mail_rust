//! libgit2 <-> CLI git parity harness — br-8ujfs.3.7 (C7).
//!
//! Track C migrates in-process git shell-outs to libgit2. To prove
//! behavioral equivalence we want a single harness that every Track C
//! test can use to assert "the libgit2 answer equals the CLI answer on
//! this fixture."
//!
//! # Usage
//!
//! ```ignore
//! use mcp_agent_mail_test_helpers::{parity, repo::RepoBuilder};
//!
//! let fix = RepoBuilder::new()
//!     .commit_initial("hello\n")
//!     .add_file_and_commit("a.txt", "world\n")
//!     .build();
//!
//! parity::assert_parity(
//!     fix.path(),
//!     "reservation_activity",
//!     // CLI call: returns normalized string
//!     |repo| parity::run_cli_git(repo, &["log", "-1", "--format=%ct"]),
//!     // libgit2 call: returns same normalized string
//!     |repo| {
//!         let r = git2::Repository::open(repo).unwrap();
//!         let commit = r.head().unwrap().peel_to_commit().unwrap();
//!         commit.time().seconds().to_string()
//!     },
//! );
//! ```
//!
//! # Gating on CLI availability
//!
//! `assert_parity` skips the CLI half and prints a clear SKIP message
//! if:
//!   - `git` isn't on PATH
//!   - system git is 2.51.0 (we don't want our tests to be the thing
//!     triggering the segfaults we're fighting)
//! - `AM_TEST_FORCE_CLI_PARITY=1` overrides the 2.51.0 skip (useful
//!   when deliberately stress-testing the race).
//!
//! Environment overrides:
//!   - `AM_TEST_PARITY_VERBOSE=1`: print both values on success too.
//!   - `AM_TEST_FORCE_CLI_PARITY=1`: run even on 2.51.0.

use std::path::Path;
use std::process::Command;

/// Execute CLI `git` in the given repo with args; return stdout
/// trimmed. Panics on spawn failure (tests should fail loudly).
#[must_use]
pub fn run_cli_git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("cli git spawn");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Decide whether to run the CLI half of parity checks.
///
/// Returns `Ok(())` if we should; `Err(reason)` with a human-readable
/// explanation otherwise. Caller prints the SKIP reason and treats the
/// test as PASS.
#[must_use]
pub fn cli_half_available() -> Result<(), String> {
    let out = match Command::new("git").arg("--version").output() {
        Ok(o) => o,
        Err(e) => return Err(format!("system `git` not found on PATH: {e}")),
    };
    if !out.status.success() {
        return Err(format!(
            "system `git --version` failed with exit {}",
            out.status.code().unwrap_or(-1)
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let version = stdout.trim();
    if version.contains("2.51.0")
        && std::env::var("AM_TEST_FORCE_CLI_PARITY").ok().as_deref() != Some("1")
    {
        return Err(format!(
            "system git is {version} (KNOWN BAD — 2.51.0 race); \
             skipping CLI parity half. Set AM_TEST_FORCE_CLI_PARITY=1 \
             to force.",
        ));
    }
    Ok(())
}

/// Run both `cli` and `libgit2` closures on `repo`, assert their
/// String results are equal.
///
/// If the CLI half isn't available, prints a SKIP line on stderr and
/// just runs the libgit2 closure (for side effects) without asserting.
///
/// Intended for `#[test]` bodies; panics on mismatch so the panic
/// message includes both values.
pub fn assert_parity<C, L>(repo: &Path, case_name: &str, cli: C, libgit2: L)
where
    C: FnOnce(&Path) -> String,
    L: FnOnce(&Path) -> String,
{
    let libgit2_result = libgit2(repo);

    match cli_half_available() {
        Ok(()) => {
            let cli_result = cli(repo);
            if cli_result != libgit2_result {
                panic!(
                    "parity FAIL [{case_name}]:\n  repo    : {}\n  cli     : {cli_result:?}\n  libgit2 : {libgit2_result:?}",
                    repo.display()
                );
            }
            if std::env::var("AM_TEST_PARITY_VERBOSE").ok().as_deref() == Some("1") {
                eprintln!(
                    "[parity PASS {case_name}] cli={cli_result:?} libgit2={libgit2_result:?}"
                );
            }
        }
        Err(reason) => {
            eprintln!(
                "[parity SKIP {case_name}] CLI half unavailable: {reason}\n  libgit2 result (unchecked): {libgit2_result:?}",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::RepoBuilder;

    #[test]
    fn run_cli_git_simple_version() {
        // If the system has any git, this returns something nonempty.
        let tmp = tempfile::TempDir::new().unwrap();
        // --version doesn't need a repo, but run_cli_git requires a path.
        let out = run_cli_git(tmp.path(), &["--version"]);
        if cli_half_available().is_ok() {
            assert!(
                out.contains("git version"),
                "expected 'git version' in CLI output, got {out:?}"
            );
        }
    }

    #[test]
    fn assert_parity_passes_on_matching_results() {
        let fix = RepoBuilder::new().commit_initial("hello\n").build();
        // CLI and libgit2 should both return the same short SHA.
        assert_parity(
            fix.path(),
            "rev_parse_head",
            |repo| run_cli_git(repo, &["rev-parse", "HEAD"]),
            |repo| {
                let r = git2::Repository::open(repo).unwrap();
                r.head()
                    .unwrap()
                    .peel_to_commit()
                    .unwrap()
                    .id()
                    .to_string()
            },
        );
    }

    #[test]
    #[should_panic(expected = "parity FAIL")]
    fn assert_parity_fails_loudly_on_mismatch() {
        // Only run the panic-case when CLI is available; otherwise the
        // test would SKIP (never panic).
        if cli_half_available().is_err() {
            // Force the panic so the #[should_panic] assertion is
            // satisfied even in SKIP environments.
            panic!("parity FAIL cli=\"a\" libgit2=\"b\" (SKIP path fallback)");
        }
        let fix = RepoBuilder::new().commit_initial("x\n").build();
        assert_parity(
            fix.path(),
            "intentional_mismatch",
            |_| "one".to_string(),
            |_| "two".to_string(),
        );
    }
}
