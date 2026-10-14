//! Exercise the turn executor with the real live-DB/archive reconciler.
//! The injected clock removes machine-speed dependence; these are correctness
//! tests, not a throughput benchmark or the release-smoke swarm verdict.

use super::*;
use asupersync::Outcome;
use fastmcp_core::block_on;
use mcp_agent_mail_db::{DbPoolConfig, create_pool, now_micros, queries};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::Ordering;

struct Fixture {
    cx: Cx,
    pool: DbPool,
    config: Config,
    // Keep the source and archive alive until after the pool is dropped.
    _temp: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("catch-up.sqlite3");
    let storage_root = temp.path().join("archive");
    std::fs::create_dir_all(&storage_root).unwrap();
    let pool_config = DbPoolConfig {
        database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&db_path),
        storage_root: Some(storage_root.clone()),
        min_connections: 1,
        max_connections: 1,
        ..DbPoolConfig::default()
    };
    let pool = create_pool(&pool_config).unwrap();
    let cx = Cx::for_testing();
    let project_root = temp.path().join("project");
    std::fs::create_dir(&project_root).unwrap();
    let human_key = project_root.to_string_lossy().into_owned();
    let project = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
        &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1")],
        || match block_on(queries::ensure_project(&cx, &pool, &human_key)) {
            Outcome::Ok(project) => project,
            other => panic!("ensure_project failed: {other:?}"),
        },
    );
    let project_id = project.id.unwrap();
    let mut agents = Vec::new();
    for name in ["BlueLake", "GreenStone"] {
        let agent = match block_on(queries::register_agent(
            &cx, &pool, project_id, name, "test", "test", None, None, None,
        )) {
            Outcome::Ok(agent) => agent,
            other => panic!("register_agent failed: {other:?}"),
        };
        agents.push(agent.id.unwrap());
    }
    let conn = match block_on(pool.acquire(&cx)) {
        Outcome::Ok(conn) => conn,
        other => panic!("acquire failed: {other:?}"),
    };
    // These rows model mail accepted just before a crash: inside the ordinary
    // 30-second grace, but before the restarted worker's settled-write anchor.
    let created_ts = now_micros().saturating_sub(1_000_000);
    for id in 9001_i64..=9009 {
        conn.execute_raw(&format!(
            "INSERT INTO messages \
             (id, project_id, sender_id, thread_id, subject, body_md, importance, \
              ack_required, created_ts, attachments, archive_metadata_json) \
             VALUES ({id}, {project_id}, {}, 'burst-thread', 'subject-{id}', \
                     'body-{id}', 'normal', 1, {created_ts}, '[]', '{{}}')",
            agents[0]
        ))
        .unwrap();
        conn.execute_raw(&format!(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) \
             VALUES ({id}, {}, 'to', NULL, NULL)",
            agents[1]
        ))
        .unwrap();
    }
    drop(conn);
    Fixture {
        cx,
        pool,
        config: Config {
            database_url: pool_config.database_url,
            storage_root,
            ..Config::default()
        },
        _temp: temp,
    }
}

fn assert_source_untouched(fixture: &Fixture) {
    let conn = match block_on(fixture.pool.acquire(&fixture.cx)) {
        Outcome::Ok(conn) => conn,
        other => panic!("acquire failed: {other:?}"),
    };
    let rows = conn
        .query_sync("SELECT id, subject, body_md FROM messages ORDER BY id", &[])
        .unwrap();
    assert_eq!(rows.len(), 9, "repair must neither delete nor redeliver mail");
    for (row, id) in rows.iter().zip(9001_i64..=9009) {
        assert_eq!(row.get_named::<i64>("id").unwrap(), id);
        assert_eq!(
            row.get_named::<String>("subject").unwrap(),
            format!("subject-{id}")
        );
        assert_eq!(
            row.get_named::<String>("body_md").unwrap(),
            format!("body-{id}")
        );
    }
    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS n FROM message_recipients \
             WHERE read_ts IS NULL AND ack_ts IS NULL",
            &[],
        )
        .unwrap();
    assert_eq!(rows[0].get_named::<i64>("n").unwrap(), 9);
}

fn assert_committed_bundles(fixture: &Fixture, expected_messages: usize) {
    let mut copies = BTreeMap::<i64, usize>::new();
    for entry in walkdir::WalkDir::new(&fixture.config.storage_root).follow_links(false) {
        let entry = entry.unwrap();
        if !entry.file_type().is_file()
            || entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "md")
        {
            continue;
        }
        let bytes = std::fs::read(entry.path()).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let Some((frontmatter, body)) = text
            .strip_prefix("---json\n")
            .and_then(|text| text.split_once("\n---\n\n"))
        else {
            continue;
        };
        let message: Value = serde_json::from_str(frontmatter).unwrap();
        let Some(id) = message["id"].as_i64() else {
            continue;
        };
        assert!((9001..=9009).contains(&id));
        assert_eq!(message["subject"], json!(format!("subject-{id}")));
        assert_eq!(body, format!("body-{id}"));
        assert_eq!(message["thread_id"], json!("burst-thread"));
        assert_eq!(message["to"], json!(["GreenStone"]));
        assert_eq!(message["bcc"], json!([]));
        assert_eq!(message["ack_required"], json!(true));
        assert!(message.get("reply_to").is_none());
        // cat-file reads HEAD's raw blob, not the index or working tree. Using
        // the file's directory also covers either supported archive Git layout.
        let output = Command::new("git")
            .arg("-C")
            .arg(entry.path().parent().unwrap())
            .args(["cat-file", "blob"])
            .arg(format!("HEAD:./{}", entry.file_name().to_string_lossy()))
            .output()
            .expect("Git is required for archive integration tests");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, bytes, "disk and committed archive must agree");
        *copies.entry(id).or_default() += 1;
    }
    assert_eq!(copies.len(), expected_messages);
    assert!(
        copies.values().all(|copies| *copies == 3),
        "canonical, outbox and inbox copies are required"
    );
}

#[test]
fn real_crash_orphans_converge_across_checkpointed_batches_without_client_reads() {
    let fixture = fixture();
    let stop = AtomicBool::new(false);
    let mut cursor = ReconcileCursor::settled_before(now_micros());
    let mut reports = Vec::new();
    let backlog = run_with(
        || reconcile_message_batch(&fixture.cx, &fixture.pool, &fixture.config, &mut cursor, &stop),
        |report| reports.push((report.repaired, report.deferred, report.checkpoint_saved)),
        || Duration::ZERO,
    )
    .unwrap();
    assert!(!backlog);
    assert!(
        reports.len() >= 3,
        "nine repairs require multiple four-repair batches"
    );
    assert_eq!(reports.iter().map(|report| report.0).sum::<usize>(), 9);
    assert!(
        reports
            .iter()
            .all(|report| report.0 <= 4 && report.1 == 0 && report.2 == Some(true))
    );
    assert_source_untouched(&fixture);
    assert_committed_bundles(&fixture, 9);
}

#[test]
fn shutdown_between_real_batches_leaves_remaining_source_mail_untouched() {
    let fixture = fixture();
    let stop = AtomicBool::new(false);
    let mut cursor = ReconcileCursor::settled_before(now_micros());
    let mut observed = Vec::new();
    let backlog = run_with(
        || reconcile_message_batch(&fixture.cx, &fixture.pool, &fixture.config, &mut cursor, &stop),
        |report| {
            observed.push((report.repaired, report.interrupted));
            stop.store(true, Ordering::Release);
        },
        || Duration::ZERO,
    )
    .unwrap();
    assert!(!backlog, "shutdown must not request catch-up polling");
    assert_eq!(observed, [(4, false), (0, true)]);
    assert_source_untouched(&fixture);
    assert_committed_bundles(&fixture, 4);
}
