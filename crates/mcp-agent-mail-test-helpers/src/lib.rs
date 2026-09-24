//! Shared test helpers for the git 2.51.0 hardening epic (br-8ujfs).
//!
//! # What this crate provides
//!
//! - [`shim_git`]: builders for fake `git` binaries with controlled
//!   version output, exit behavior, and delays.
//! - [`repo`]: factory for tempdir repos in every shape we care about
//!   (empty, with commits, with orphan stash, bare, worktree, etc.).
//!
//! # Why a dedicated crate
//!
//! Tracks A-F across the epic each need 3-5 test files that all build
//! the same fixture scaffolding. Centralizing here prevents drift and
//! deduplicates ~500 lines of inline scaffolding.
//!
//! # Scope
//!
//! - dev-dependency ONLY. Production code must never pull this in.
//! - Safe on Unix and Windows; shim-script builder produces `.bat` on
//!   Windows.
//! - Uses `tempfile::TempDir` for guaranteed cleanup.

#![forbid(unsafe_code)]

pub mod parity;
pub mod repo;
pub mod shim_git;

// Re-export so callers can `use mcp_agent_mail_test_helpers::*` if they
// prefer.
pub use repo::{RepoBuilder, RepoFixture};
pub use shim_git::{ShimBehavior, ShimExit, build_shim_git};

/// Libtest path of test `$name` defined in the calling module (br-kp1in.28).
///
/// Integration-test files are modules of their crate's single test binary, so
/// a test that re-executes its own binary with `--exact` must pass
/// `<module>::<name>`: a bare name matches nothing, and libtest then exits 0
/// after running zero tests.
#[macro_export]
macro_rules! libtest_path {
    ($name:expr) => {
        match ::core::module_path!().split_once("::") {
            ::core::option::Option::Some((_crate, module)) => {
                ::std::format!("{module}::{}", $name)
            }
            ::core::option::Option::None => ::std::string::String::from($name),
        }
    };
}

/// Integration-test files in `tests_dir` that the consolidated test binary's
/// root (`root_stem`, e.g. `it` for `tests/it.rs`) does not declare as a
/// `mod <stem>;` line (br-kp1in.28).
///
/// Crates link all integration tests into ONE binary (`autotests = false` plus
/// `tests/it.rs`), because every test binary statically contains the whole
/// dependency graph (~0.7 GB each). A new `tests/<name>.rs` missing from the
/// root would then never compile or run; each root asserts this is empty.
///
/// # Panics
///
/// Panics if `tests_dir` cannot be read.
#[must_use]
pub fn unlinked_test_files(
    tests_dir: &std::path::Path,
    root_stem: &str,
    root_src: &str,
) -> Vec<String> {
    let declared: std::collections::BTreeSet<&str> = root_src
        .lines()
        .filter_map(|line| line.trim().strip_prefix("mod ")?.strip_suffix(';'))
        .map(str::trim)
        .collect();
    let mut missing: Vec<String> = std::fs::read_dir(tests_dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", tests_dir.display()))
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()? != "rs" {
                return None;
            }
            let stem = path.file_stem()?.to_str()?.to_string();
            (stem != root_stem && !declared.contains(stem.as_str())).then_some(stem)
        })
        .collect();
    missing.sort();
    missing
}

#[cfg(test)]
mod tests {
    use super::unlinked_test_files;

    #[test]
    fn libtest_path_is_the_module_qualified_test_name() {
        // A unit test here is addressed by libtest as `tests::<name>`.
        assert_eq!(crate::libtest_path!("probe"), "tests::probe");
    }

    #[test]
    fn unlinked_test_files_reports_only_undeclared_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["it.rs", "alpha.rs", "beta.rs", "notes.txt"] {
            std::fs::write(dir.path().join(name), "").expect("write fixture");
        }
        std::fs::create_dir(dir.path().join("common")).expect("helper dir");
        let root = "//! root\n#[cfg(unix)]\nmod alpha;\nmod common;\n";
        assert_eq!(unlinked_test_files(dir.path(), "it", root), vec!["beta"]);
        let root = format!("{root}mod beta;\n");
        assert_eq!(
            unlinked_test_files(dir.path(), "it", &root),
            Vec::<String>::new()
        );
    }
}
