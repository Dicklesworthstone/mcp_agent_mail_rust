#![allow(clippy::missing_panics_doc)]

use std::path::Path;

use mcp_agent_mail_core::config::Config;
use mcp_agent_mail_storage::{
    ensure_archive, flush_async_commits, get_recent_commits, list_agent_inbox, read_message_file,
    write_message_bundle,
};
use tempfile::TempDir;

fn test_config(root: &Path) -> Config {
    Config {
        storage_root: root.to_path_buf(),
        ..Config::default()
    }
}

#[test]
fn archive_target_message_bundle_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let config = test_config(tmp.path());
    let archive = ensure_archive(&config, "archive-target")?;
    let recipients = vec!["RedHarbor".to_string()];
    let message = serde_json::json!({
        "id": 42,
        "subject": "Archive Target",
        "thread_id": "br-archive",
        "created_ts": "2026-04-22T00:00:00Z",
    });

    write_message_bundle(
        &archive,
        &config,
        &message,
        "archive target body",
        "BlueLake",
        &recipients,
        &[],
        Some("archive target smoke"),
    )?;
    flush_async_commits();

    let inbox = list_agent_inbox(&archive, "RedHarbor")?;
    assert_eq!(inbox.len(), 1);

    let (frontmatter, body) = read_message_file(&inbox[0])?;
    assert_eq!(frontmatter["subject"], "Archive Target");
    assert_eq!(frontmatter["thread_id"], "br-archive");
    assert_eq!(body, "archive target body");

    let commits = get_recent_commits(&archive, 5, None)?;
    assert!(
        commits
            .iter()
            .any(|commit| commit.summary == "archive target smoke"),
        "missing archive smoke commit in {commits:?}",
    );

    Ok(())
}
