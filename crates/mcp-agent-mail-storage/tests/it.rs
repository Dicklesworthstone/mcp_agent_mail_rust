//! Every integration test of this crate, linked as ONE test binary (br-kp1in.28).
//!
//! Each integration-test binary statically contains the whole dependency graph
//! (~0.7 GB at line-tables-only), so one binary per file made the full gate
//! ~100 GB. Each former `tests/<name>.rs` binary is now module `<name>` here,
//! so its tests are addressed as `<name>::<test>`; nextest still runs every
//! test in its own process. Declare new test files below;
//! `every_test_file_is_linked` fails for any `tests/*.rs` left out.

mod archive;
mod archive_batch_write_schema;
mod atc_retention_soak;
mod fsync_matrix;
mod libgit2_index_race_immunity;
mod stress_pipeline;
mod stress_pipeline_known_bad_git;

#[test]
fn default_archive_root_is_refused_in_this_test_binary() {
    // The consolidated binary must still be recognised as a test harness from
    // its REAL environment (no harness marker is injected here), so an
    // integration test that resolves the operator's default archive is refused
    // (br-99aih) instead of writing to it.
    assert!(
        mcp_agent_mail_core::config::is_running_under_cargo_test_harness(),
        "the consolidated integration-test binary is not detected as a test harness"
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let xdg_data = tmp.path().join("xdg-data");
    std::fs::create_dir_all(&home).expect("isolated home");
    std::fs::create_dir_all(&xdg_data).expect("isolated xdg data");
    let home = home.to_string_lossy().into_owned();
    let xdg_data = xdg_data.to_string_lossy().into_owned();
    let attempt = |allow: &str| {
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[
                ("HOME", home.as_str()),
                ("USERPROFILE", home.as_str()),
                ("XDG_DATA_HOME", xdg_data.as_str()),
                ("AM_ALLOW_HOME_STORAGE_ROOT", allow),
            ],
            || {
                let default_root = mcp_agent_mail_core::config::default_storage_root_path();
                assert!(
                    default_root.starts_with(tmp.path()),
                    "the default root {} must be redirected into the tempdir",
                    default_root.display()
                );
                let config = mcp_agent_mail_core::Config {
                    storage_root: default_root.clone(),
                    ..mcp_agent_mail_core::Config::default()
                };
                (
                    mcp_agent_mail_storage::ensure_archive_root(&config).is_ok(),
                    default_root.exists(),
                )
            },
        )
    };
    assert_eq!(
        attempt(""),
        (false, false),
        "the default archive root must be refused before anything is created"
    );
    // Control: the documented opt-out admits the same call.
    assert_eq!(attempt("1"), (true, true));
}

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
