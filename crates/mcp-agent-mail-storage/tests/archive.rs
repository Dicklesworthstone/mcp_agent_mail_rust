#![allow(clippy::missing_panics_doc)]

use std::path::Path;

use mcp_agent_mail_core::config::Config;
use mcp_agent_mail_storage::{
    commit_paths_with_retry, ensure_archive, flush_async_commits, get_recent_commits,
    list_agent_inbox, read_message_file, write_message_bundle,
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

#[test]
fn recent_commits_filter_returns_latest_matching_change() -> Result<(), Box<dyn std::error::Error>>
{
    let tmp = TempDir::new()?;
    let config = test_config(tmp.path());
    let archive = ensure_archive(&config, "archive-filter")?;
    let recipients = vec!["RedHarbor".to_string()];
    for (id, sender, subject) in [(1, "BlueLake", "first"), (2, "RedHarbor", "second")] {
        let message = serde_json::json!({
            "id": id,
            "subject": subject,
            "thread_id": "br-filter",
            "created_ts": "2026-04-22T00:00:00Z",
        });
        write_message_bundle(
            &archive,
            &config,
            &message,
            subject,
            sender,
            &recipients,
            &[],
            Some(subject),
        )?;
        flush_async_commits();
    }
    let all = get_recent_commits(&archive, 1, None)?;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].summary, "second");
    let sender = get_recent_commits(&archive, 5, Some("agents/BlueLake"))?;
    assert!(sender.iter().any(|c| c.summary == "first"));
    assert!(!sender.iter().any(|c| c.summary == "second"));
    Ok(())
}

/// GH#362: lock-free commits give their repository handle an empty in-memory
/// index so blob filtering no longer parses the whole on-disk index. The
/// archive's `.gitattributes` must still apply: `*.md text eol=lf` keeps
/// normalizing CRLF bodies to LF in the committed blob.
#[test]
fn archive_commits_still_apply_gitattributes_line_ending_normalization()
-> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let config = test_config(tmp.path());
    let archive = ensure_archive(&config, "archive-eol")?;
    let rel_path = format!("projects/{}/notes/crlf.md", archive.slug);
    let full_path = archive.repo_root.join(&rel_path);
    std::fs::create_dir_all(full_path.parent().expect("note directory"))?;
    std::fs::write(&full_path, b"line one\r\nline two\r\n")?;

    commit_paths_with_retry(
        &archive.repo_root,
        &config,
        "eol normalization check",
        &[rel_path.as_str()],
    )?;

    let repo = git2::Repository::open(&archive.repo_root)?;
    let head = repo.head()?.peel_to_commit()?;
    assert_eq!(head.summary()?, Some("eol normalization check"));
    let entry = head.tree()?.get_path(Path::new(&rel_path))?;
    let blob = repo.find_blob(entry.id())?;
    assert_eq!(blob.content(), b"line one\nline two\n");
    // The working-tree file is the writer's, not git's: it stays as written.
    assert_eq!(std::fs::read(&full_path)?, b"line one\r\nline two\r\n");
    Ok(())
}
