//! Reliability unit-test coverage gate (br-bvq1x.14.1 / N1).
//!
//! Enforces the unit-test Definition-of-Done for the `br-bvq1x` reliability
//! program:
//!   1. Every registered reliability module exists and carries at least one
//!      inline **error-path** test (`is_err`/`unwrap_err`/`should_panic`/`Err(...)`
//!      or an error-named test).
//!   2. The committed `docs/RELIABILITY_COVERAGE_MATRIX.md` stays in sync with
//!      the live registry. Regenerate with `UPDATE_GOLDEN=1`.
//!
//! Wired into `am ci` (gate "Reliability test-coverage matrix") and
//! `am verify reliability-coverage` (lane). Pure source scan — no build needed
//! beyond compiling the test.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use mcp_agent_mail_cli::reliability_coverage::{
    scan_modules, workspace_root_from_manifest,
};

fn workspace_root() -> PathBuf {
    workspace_root_from_manifest()
}

fn matrix_path() -> PathBuf {
    workspace_root().join("docs/RELIABILITY_COVERAGE_MATRIX.md")
}

fn bless_requested() -> bool {
    std::env::var_os("UPDATE_GOLDEN").is_some() || std::env::var_os("UPDATE_GOLDENS").is_some()
}

#[test]
fn every_reliability_module_has_an_error_path_test() {
    let root = workspace_root();
    let report = scan_modules(&root);

    if !report.is_pass() {
        let mut msg = String::from(
            "br-bvq1x.14.1 (N1): the following tracked reliability modules fail the \
             unit-test coverage standard (each must carry >=1 inline error-path test):\n",
        );
        for m in report.gaps() {
            let gap = m
                .gap
                .map(|g| g.describe())
                .unwrap_or("unknown gap");
            msg.push_str(&format!(
                "  - [{}] {} :: {} ({} tests found)\n",
                m.track, m.rel_path, gap, m.scan.total_tests
            ));
        }
        msg.push_str(
            "Add a meaningful error/degraded-path test to each module above, or update the \
             registry in crates/mcp-agent-mail-cli/src/reliability_coverage.rs if a module \
             was renamed/removed.",
        );
        panic!("{msg}");
    }

    // Sanity: the registry is non-trivial and the report covers it fully.
    assert!(
        report.modules.len() >= 10,
        "reliability registry unexpectedly small ({} modules)",
        report.modules.len()
    );
    assert_eq!(report.covered_count(), report.modules.len());
}

#[test]
fn committed_coverage_matrix_is_in_sync() {
    let root = workspace_root();
    let report = scan_modules(&root);
    let expected = report.render_markdown();
    let path = matrix_path();

    if bless_requested() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create docs dir");
        }
        std::fs::write(&path, &expected).expect("write coverage matrix");
        eprintln!("blessed {}", path.display());
        return;
    }

    let actual = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing {} ({e}); generate it with \
             `UPDATE_GOLDEN=1 cargo test -p mcp-agent-mail-cli --test reliability_coverage_ci`",
            path.display()
        )
    });

    assert_eq!(
        actual, expected,
        "docs/RELIABILITY_COVERAGE_MATRIX.md is stale; regenerate with \
         `UPDATE_GOLDEN=1 cargo test -p mcp-agent-mail-cli --test reliability_coverage_ci`"
    );
}
