//! Every integration test of this crate, linked as ONE test binary (br-kp1in.28).
//!
//! Each integration-test binary statically contains the whole dependency graph
//! (~0.7 GB at line-tables-only), so one binary per file made the full gate
//! ~100 GB. Each former `tests/<name>.rs` binary is now module `<name>` here,
//! so its tests are addressed as `<name>::<test>`; nextest still runs every
//! test in its own process. Declare new test files below;
//! `every_test_file_is_linked` fails for any `tests/*.rs` left out.

/// Shared helpers (tests/common/mod.rs); use them as `crate::common`.
mod common;

mod archive_search_consistency;
mod atc_experience_lifecycle;
mod atc_leader_lease;
mod atc_rollup_snapshot;
mod cache_golden;
mod canonical_schema_readable;
mod coalesce_stress;
mod diag_hang;
mod diversity_dedup;
mod evidence_wiring;
mod fault_injection;
mod filter_pagination;
mod forensic_bundle_replay;
mod frankensqlite_pragma_conformance;
mod golden_ranking;
mod idempotency_integration;
mod identity_fts_cleanup;
mod load_bench;
mod load_concurrency;
mod logging_redaction;
mod loom_coalesce;
mod mail_explorer;
mod message_id_multiprocess;
mod migration_tests;
mod nocase_backup_compat;
mod parser_filter_fusion_rerank;
mod pool_exhaustion;
mod query_assistance_explain;
mod query_integration;
mod relevance_harness;
mod s3fifo_bug;
mod schema_invariants;
mod schema_migration;
mod scope_policy_property;
mod search_benchmark;
mod search_conformance_fuzz;
mod search_planner_unit;
mod search_quality;
mod search_v3_conformance;
mod startup_page_leak;
mod stress;
mod sustained_load;
mod timeout_backpressure;

#[test]
fn every_test_file_is_linked() {
    let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let missing =
        mcp_agent_mail_test_helpers::unlinked_test_files(&tests_dir, "it", include_str!("it.rs"));
    assert!(
        missing.is_empty(),
        "tests/*.rs not declared in tests/it.rs (add `mod <name>;`): {missing:?}"
    );
}
