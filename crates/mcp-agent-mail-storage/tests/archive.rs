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

/// Own only the child started by this test. Closing stdin lets Git publish its
/// staged index; every failure/panic also reaps it instead of leaving a writer
/// and its index.lock behind in a surviving process.
struct LiveGitIndexWriter(std::process::Child);

impl LiveGitIndexWriter {
    fn start(root: &Path) -> Self {
        let child = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["update-index", "--index-info"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("start actual Git index writer");
        let mut writer = Self(child);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !root.join(".git/index.lock").is_file() {
            assert!(
                writer.0.try_wait().unwrap().is_none(),
                "Git exited before locking"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "Git did not acquire its index lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        writer
    }

    fn stage(&mut self, name: &str, blob: git2::Oid) {
        use std::io::Write as _;
        let input = self.0.stdin.as_mut().expect("live Git writer stdin");
        writeln!(input, "100644 {blob}\t{name}").unwrap();
        input.flush().unwrap();
    }

    fn finish(&mut self) {
        drop(self.0.stdin.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "Git index writer failed: {status}");
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Git index writer did not exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

impl Drop for LiveGitIndexWriter {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct StopConcurrentReaders<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for StopConcurrentReaders<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Keep latency evidence bounded even during the 45-minute qualification run.
/// Buckets round up to milliseconds, so reported quantiles never round down.
#[derive(Default)]
struct InboxLatencyMeasurements {
    count: usize,
    maximum: std::time::Duration,
    millis: std::collections::BTreeMap<u128, usize>,
}

impl InboxLatencyMeasurements {
    fn observe(&mut self, elapsed: std::time::Duration) {
        self.count += 1;
        self.maximum = self.maximum.max(elapsed);
        *self
            .millis
            .entry(elapsed.as_nanos().div_ceil(1_000_000))
            .or_default() += 1;
    }

    fn percentile_ms(&self, percentile: usize) -> Option<u128> {
        let rank = self.count.checked_mul(percentile)?.div_ceil(100);
        if rank == 0 {
            return None;
        }
        let mut observations = 0;
        self.millis.iter().find_map(|(millis, count)| {
            observations += count;
            (observations >= rank).then_some(*millis)
        })
    }
}

#[derive(Default)]
struct ReconciliationMeasurements {
    scanned: usize,
    unchanged: usize,
    deferred_transient: usize,
    peak_retry_len: usize,
    budget_exhausted_batches: usize,
    live_git_lock_checks: usize,
    maximum_batch: std::time::Duration,
    first_repair_ms: Option<u128>,
}

fn archive_test_outcome<T, E: std::fmt::Debug>(value: asupersync::Outcome<T, E>) -> T {
    match value {
        asupersync::Outcome::Ok(value) => value,
        asupersync::Outcome::Err(error) => panic!("database operation failed: {error:?}"),
        asupersync::Outcome::Cancelled(reason) => {
            panic!("database operation cancelled: {reason:?}")
        }
        asupersync::Outcome::Panicked(error) => panic!("database operation panicked: {error}"),
    }
}

fn reconciliation_mailbox_evidence(
    cx: &asupersync::Cx,
    pool: &mcp_agent_mail_db::DbPool,
) -> serde_json::Value {
    let conn = archive_test_outcome(fastmcp_core::block_on(pool.acquire(cx)));
    let messages = conn.query_sync(
        "SELECT id, project_id, sender_id, subject, body_md, thread_id, topic, importance, ack_required, \
         created_ts, recipients_json, attachments, archive_metadata_json FROM messages ORDER BY id",
        &[],
    ).unwrap();
    let messages = messages.iter().map(|row| serde_json::json!({
        "id": row.get_named::<i64>("id").unwrap(),
        "project_id": row.get_named::<i64>("project_id").unwrap(),
        "sender_id": row.get_named::<i64>("sender_id").unwrap(),
        "subject": row.get_named::<String>("subject").unwrap(),
        "body_md": row.get_named::<String>("body_md").unwrap(),
        "thread_id": row.get_named::<Option<String>>("thread_id").unwrap(),
        "topic": row.get_named::<Option<String>>("topic").unwrap(),
        "importance": row.get_named::<String>("importance").unwrap(),
        "ack_required": row.get_named::<i64>("ack_required").unwrap(),
        "created_ts": row.get_named::<i64>("created_ts").unwrap(),
        "recipients_json": row.get_named::<String>("recipients_json").unwrap(),
        "attachments": row.get_named::<String>("attachments").unwrap(),
        "archive_metadata_json": row.get_named::<Option<String>>("archive_metadata_json").unwrap(),
    })).collect::<Vec<_>>();
    let receipts = conn.query_sync(
        "SELECT message_id, agent_id, kind, read_ts, ack_ts FROM message_recipients ORDER BY message_id, agent_id",
        &[],
    ).unwrap();
    let receipts = receipts
        .iter()
        .map(|row| {
            serde_json::json!({
                "message_id": row.get_named::<i64>("message_id").unwrap(),
                "agent_id": row.get_named::<i64>("agent_id").unwrap(),
                "kind": row.get_named::<String>("kind").unwrap(),
                "read_ts": row.get_named::<Option<i64>>("read_ts").unwrap(),
                "ack_ts": row.get_named::<Option<i64>>("ack_ts").unwrap(),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({"messages": messages, "receipts": receipts})
}

fn reconciliation_message_body(id: i64, large_mailbox: bool) -> String {
    let mut body = format!("Keep message {id}: λ.");
    if large_mailbox {
        let detail = format!(
            "\nMessage {id} recovery evidence: preserve recipients, read and ACK state, reply metadata, \
             and the exact body while concurrent archive and inbox operations continue."
        );
        while body.len() < 3 * 1024 {
            body.push_str(&detail);
        }
        // Only the initial line contains non-ASCII text; this cut is within
        // the ASCII detail, so the approximately 3 KiB body stays valid UTF-8.
        body.truncate(3 * 1024);
    }
    body
}

fn reconciliation_message_created_ts(id: i64, large_mailbox: bool) -> i64 {
    if large_mailbox {
        // Historical hourly traffic spans several archive months instead of
        // putting 25,100 real Git tree entries in one synthetic 26-second run.
        1_672_531_200_000_000 + id * 3_600_000_000
    } else {
        1_000_000 + id * 1000
    }
}

/// GH#374: real Git index ownership must not keep crash orphans pending while
/// ordinary inbox reads and writes continue. The default exercises the actual
/// concurrent archive/DB/checkpoint protocols on 96 messages. Set
/// `AM_TEST_GH374_LARGE_MAILBOX=1` to qualify the same integrity and latency
/// assertions on 25,100 actual messages with 3 KiB bodies.
#[test]
fn gh374_reconciliation_converges_with_live_git_writer_and_inbox_readers() {
    const CHILD: &str = "AM_TEST_GH374_ARCHIVE_CONCURRENCY_CHILD";
    let thread = std::thread::current();
    let name = thread.name().expect("named integration test");
    if std::env::var(CHILD).as_deref() != Ok(name) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            // Progress must remain observable before a large child finishes.
            // Keep stdout captured to retain the exact-test execution check.
            .stderr(std::process::Stdio::inherit())
            .output()
            .expect("run isolated real-concurrency test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "isolated concurrency test failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        print!("{stdout}");
        return;
    }
    mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
        run_reconciliation_with_live_git_writer_and_inbox_readers();
    });
}

fn run_reconciliation_with_live_git_writer_and_inbox_readers() {
    use asupersync::{Cx, runtime::RuntimeBuilder};
    use mcp_agent_mail_db::{DbPool, DbPoolConfig, queries};
    use mcp_agent_mail_storage::recovery::message_reconcile::database::{
        ReconcileCursor, reconcile_message_batch,
    };
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    const FIRST_ID: i64 = 901;
    const SEED_BATCH_SIZE: usize = 128;
    let large_mailbox = std::env::var("AM_TEST_GH374_LARGE_MAILBOX").as_deref() == Ok("1");
    let message_count = if large_mailbox { 25_100 } else { 96 };
    let end_id = FIRST_ID + i64::try_from(message_count).unwrap();
    let expected_newest_ids = [end_id - 1, end_id - 2, end_id - 3, end_id - 4, end_id - 5];
    let convergence_budget = Duration::from_secs(if large_mailbox { 45 * 60 } else { 120 });
    let pass_limit = if large_mailbox {
        4 * message_count
    } else {
        4096
    };
    let temp = TempDir::new().unwrap();
    let config = Config {
        storage_root: temp.path().join("archive"),
        database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&temp.path().join("mail.db")),
        ..Config::default()
    };
    // Create the project's archive directory only after seeding: opening the
    // pool registers any project it finds there, and the seed needs id 101.
    std::fs::create_dir_all(&config.storage_root).unwrap();
    let pool_config = DbPoolConfig {
        database_url: config.database_url.clone(),
        storage_root: Some(config.storage_root.clone()),
        min_connections: 1,
        max_connections: 4,
        ..Default::default()
    };
    let pool = mcp_agent_mail_db::create_pool(&pool_config).unwrap();
    let cx = Cx::for_testing();
    let conn = archive_test_outcome(fastmcp_core::block_on(pool.acquire(&cx)));
    let seed_started = Instant::now();
    conn.execute_raw(
        "INSERT INTO projects(id,slug,human_key,created_at) VALUES(101,'gh374','/gh374',1); \
         INSERT INTO agents(id,project_id,name,program,model,task_description,inception_ts,last_active_ts) \
         VALUES(101,101,'BlueLake','test','test','',1,1), \
         (102,101,'GreenStone','test','test','',1,1),(103,101,'RedFox','test','test','',1,1); \
         INSERT INTO db_identity(singleton,generation_id) VALUES(0,'aabbccddeeff0011')",
    ).unwrap();
    for first in (FIRST_ID..end_id).step_by(SEED_BATCH_SIZE) {
        let end = (first + i64::try_from(SEED_BATCH_SIZE).unwrap()).min(end_id);
        let messages = (first..end)
            .map(|id| {
                let body = reconciliation_message_body(id, large_mailbox).replace('\'', "''");
                format!(
                    "({id},101,101,'recovery-thread','handoffs','handoff-{id}','{body}', \
                     'high',1,{},'{{\"to\":[\"GreenStone\"],\"cc\":[],\"bcc\":[\"RedFox\"]}}', \
                     '[]','{{\"reply_to\":700}}')",
                    reconciliation_message_created_ts(id, large_mailbox),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let receipts = (first..end)
            .map(|id| format!("({id},102,'to',2000000,2500000),({id},103,'bcc',2000000,2500000)"))
            .collect::<Vec<_>>()
            .join(",");
        // Bound statement/transaction size while avoiding a separate durable
        // commit for each of the 25,100 messages and its receipt insert.
        conn.execute_raw(&format!(
            "BEGIN; INSERT INTO messages(id,project_id,sender_id,thread_id,topic,subject,body_md,importance, \
             ack_required,created_ts,recipients_json,attachments,archive_metadata_json) \
             VALUES {messages}; \
             INSERT INTO message_recipients(message_id,agent_id,kind,read_ts,ack_ts) \
             VALUES {receipts}; COMMIT",
        )).unwrap();
    }
    let seed_elapsed_ms = seed_started.elapsed().as_millis();
    let page_count = conn.query_sync("PRAGMA page_count", &[]).unwrap()[0]
        .get_as::<i64>(0)
        .unwrap();
    let page_size = conn.query_sync("PRAGMA page_size", &[]).unwrap()[0]
        .get_as::<i64>(0)
        .unwrap();
    let database_bytes = page_count.checked_mul(page_size).unwrap();
    eprintln!(
        "{}",
        serde_json::json!({"case":"gh374_archive_progress", "phase":"seeded",
            "messages":message_count, "database_bytes":database_bytes,
            "seed_elapsed_ms":seed_elapsed_ms})
    );
    drop(conn);
    let archive = ensure_archive(&config, "gh374").unwrap();
    let original = reconciliation_mailbox_evidence(&cx, &pool);
    assert_eq!(
        original["messages"].as_array().unwrap().len(),
        message_count
    );
    assert_eq!(
        original["receipts"].as_array().unwrap().len(),
        message_count * 2
    );
    let read_pool = DbPool::new_query_only(&DbPoolConfig {
        min_connections: 2,
        max_connections: 2,
        ..pool_config.clone()
    })
    .unwrap();

    let repo = git2::Repository::open(&config.storage_root).unwrap();
    let pending_path = "operator-pending.txt";
    std::fs::write(
        config.storage_root.join(pending_path),
        b"unpublished operator evidence\n",
    )
    .unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(pending_path)).unwrap();
    index.write().unwrap();
    let pending_oid = index.get_path(Path::new(pending_path), 0).unwrap().id;
    drop(index);
    let later_oid = repo.blob(b"second unpublished operator note\n").unwrap();
    let mut git_writer = LiveGitIndexWriter::start(&config.storage_root);
    let held_index = std::fs::read(repo.path().join("index")).unwrap();
    assert!(matches!(
        mcp_agent_mail_storage::archive_epoch_read_stamp(&config.storage_root),
        Err(mcp_agent_mail_storage::StorageError::LockContention { .. })
    ));

    let stop = AtomicBool::new(false);
    let before_deferral_head = repo.head().unwrap().target();
    // Force real, durable retry debt before the concurrency phase. A final
    // empty queue alone cannot prove that deferred work survived a restart.
    let (mut cursor, initial_retry_len, restored_retry_len) = std::thread::scope(|scope| {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let owner = scope.spawn(move || {
            mcp_agent_mail_storage::with_archive_snapshot_publication_fence(|| {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(30))
            })
        });
        // On every unwind this closure drops release_tx before scope joins
        // the owner, so an assertion cannot strand the publication fence.
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
        let deferred = reconcile_message_batch(&cx, &pool, &config, &mut cursor, &stop).unwrap();
        let checkpoints = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(".am-message-reconcile-") && name.ends_with(".json")
                    })
            })
            .collect::<Vec<_>>();
        assert_eq!(checkpoints.len(), 1, "missing durable retry checkpoint");
        let checkpoint_retry_debt = || {
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&checkpoints[0]).unwrap()).unwrap();
            saved["retries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|retry| {
                    (
                        retry["id"].as_i64().unwrap(),
                        retry["attempts"].as_u64().unwrap(),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let initial_debt = checkpoint_retry_debt();
        assert_eq!(initial_debt.len(), deferred.retry_len);
        assert!(!initial_debt.is_empty(), "no deferred work was persisted");

        cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
        let restored = reconcile_message_batch(&cx, &pool, &config, &mut cursor, &stop).unwrap();
        let restored_debt = checkpoint_retry_debt();
        release_tx.send(()).unwrap();
        assert!(
            owner.join().unwrap().is_ok(),
            "publication fence owner timed out"
        );
        for report in [&deferred, &restored] {
            assert_eq!(
                (
                    report.repaired,
                    report.files_created,
                    report.deferred_conflict
                ),
                (0, 0, 0)
            );
            assert!(
                report.deferred_transient > 0 && report.retry_len > 0,
                "{report:?}"
            );
            assert!(!report.interrupted);
            assert_eq!(report.checkpoint_saved, Some(true));
            assert!(report.scanned <= 32 && report.payload_bytes <= 16 * 1024 * 1024);
        }
        assert!(restored.checkpoint_restored, "retry debt was not restored");
        assert_eq!(restored_debt.len(), restored.retry_len);
        assert!(
            initial_debt.keys().all(|id| restored_debt.contains_key(id)),
            "restart discarded deferred IDs: {initial_debt:?} -> {restored_debt:?}"
        );
        assert!(
            initial_debt.iter().any(|(id, attempts)| {
                restored_debt
                    .get(id)
                    .is_some_and(|restored| restored > attempts)
            }),
            "restart rediscovered work without retaining its prior retry attempts"
        );
        (cursor, deferred.retry_len, restored.retry_len)
    });
    assert_eq!(repo.head().unwrap().target(), before_deferral_head);
    assert_eq!(
        std::fs::read(repo.path().join("index")).unwrap(),
        held_index
    );
    assert!(repo.path().join("index.lock").is_file());
    assert_eq!(reconciliation_mailbox_evidence(&cx, &pool), original);
    let start = Barrier::new(4);
    let started = Instant::now();
    let (read_results, ordinary_writes, passes, repairs, final_retry_len, measurements) =
        std::thread::scope(|scope| {
            let _stop_on_unwind = StopConcurrentReaders(&stop);
            let mut readers = Vec::new();
            for agent_id in [102, 103] {
                let stop = &stop;
                let start = &start;
                let read_pool = &read_pool;
                readers.push(scope.spawn(move || {
                    // A failed latency/integrity assertion must stop the other
                    // participants promptly, including in the 45-minute mode.
                    let _stop_on_exit = StopConcurrentReaders(stop);
                    let cx = Cx::for_testing();
                    let runtime = RuntimeBuilder::current_thread().build().unwrap();
                    let mut latencies = InboxLatencyMeasurements::default();
                    start.wait();
                    while !stop.load(Ordering::Acquire) {
                        let read_started = Instant::now();
                        let inbox = archive_test_outcome(runtime.block_on(queries::fetch_inbox(
                            &cx, read_pool, 101, agent_id, false, None, 5,
                        )));
                        let elapsed = read_started.elapsed();
                        assert!(
                            elapsed < Duration::from_secs(30),
                            "inbox exceeded dispatch deadline: {elapsed:?}"
                        );
                        latencies.observe(elapsed);
                        assert_eq!(
                            inbox
                                .iter()
                                .map(|row| row.message.id.unwrap())
                                .collect::<Vec<_>>(),
                            expected_newest_ids
                        );
                        for row in inbox {
                            let id = row.message.id.unwrap();
                            assert_eq!(
                                row.message.body_md,
                                reconciliation_message_body(id, large_mailbox)
                            );
                            assert_eq!(
                                (row.read_ts, row.ack_ts),
                                (Some(2_000_000), Some(2_500_000))
                            );
                            assert_eq!(row.kind, if agent_id == 102 { "to" } else { "bcc" });
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    latencies
                }));
            }
            let writer = scope.spawn(|| {
                let _stop_on_exit = StopConcurrentReaders(&stop);
                let cx = Cx::for_testing();
                let runtime = RuntimeBuilder::current_thread().build().unwrap();
                let note = "projects/gh374/ordinary-progress.txt";
                let mut writes = 0_i64;
                start.wait();
                while !stop.load(Ordering::Acquire) {
                    let write_started = Instant::now();
                    let conn = archive_test_outcome(runtime.block_on(pool.acquire(&cx)));
                    conn.execute_sync(
                        "UPDATE agents SET last_active_ts = ? WHERE id = 101",
                        &[(3_000_000 + writes).into()],
                    )
                    .unwrap();
                    drop(conn);
                    std::fs::write(
                        config.storage_root.join(note),
                        format!("ordinary write {writes}\n"),
                    )
                    .unwrap();
                    commit_paths_with_retry(
                        &config.storage_root,
                        &config,
                        "ordinary concurrent note",
                        &[note],
                    )
                    .unwrap();
                    assert!(
                        write_started.elapsed() < Duration::from_secs(30),
                        "ordinary writer starved"
                    );
                    writes += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                writes
            });
            let deadline = Instant::now() + convergence_budget;
            let mut repairs = 0;
            let mut restarted = false;
            let mut verify_restart = false;
            let mut passes = 0;
            let mut final_retry_len = None;
            let mut measurements = ReconciliationMeasurements::default();
            let mut last_progress = Instant::now();
            start.wait();
            for pass in 0..pass_limit {
                assert!(
                    Instant::now() < deadline,
                    "reconciliation did not converge after {passes} passes / {repairs} repairs"
                );
                let batch_started = Instant::now();
                let report =
                    reconcile_message_batch(&cx, &pool, &config, &mut cursor, &stop).unwrap();
                measurements.maximum_batch =
                    measurements.maximum_batch.max(batch_started.elapsed());
                measurements.scanned += report.scanned;
                measurements.unchanged += report.unchanged;
                measurements.deferred_transient += report.deferred_transient;
                measurements.peak_retry_len = measurements.peak_retry_len.max(report.retry_len);
                measurements.budget_exhausted_batches += usize::from(report.budget_exhausted);
                if report.repaired > 0 && measurements.first_repair_ms.is_none() {
                    measurements.first_repair_ms = Some(started.elapsed().as_millis());
                }
                passes = pass + 1;
                assert!(
                    report.scanned <= 32 && report.repaired <= 4,
                    "unbounded pass: {report:?}"
                );
                assert!(report.payload_bytes <= 16 * 1024 * 1024);
                assert_eq!(report.deferred_conflict, 0, "{report:?}");
                assert!(!report.interrupted);
                if verify_restart {
                    assert!(
                        report.checkpoint_restored,
                        "restart must reuse only new valid checkpoint progress"
                    );
                    verify_restart = false;
                }
                repairs += report.repaired;
                final_retry_len = Some(report.retry_len);
                assert!(
                    repo.path().join("index.lock").is_file(),
                    "repair broke active Git ownership"
                );
                measurements.live_git_lock_checks += 1;
                assert_eq!(
                    std::fs::read(repo.path().join("index")).unwrap(),
                    held_index
                );
                if repairs >= message_count / 2
                    && !restarted
                    && report.checkpoint_saved == Some(true)
                {
                    git_writer.stage("operator-later.txt", later_oid);
                    cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
                    restarted = true;
                    verify_restart = true;
                }
                if repairs == message_count && report.retry_len == 0 && !verify_restart {
                    break;
                }
                if last_progress.elapsed() >= Duration::from_secs(5) {
                    eprintln!(
                        "{}",
                        serde_json::json!({"case":"gh374_archive_progress", "phase":"reconciling",
                            "elapsed_ms":started.elapsed().as_millis(), "passes":passes,
                            "repairs":repairs, "messages":message_count,
                            "scanned":measurements.scanned, "retry_len":report.retry_len,
                            "deferred_transient":measurements.deferred_transient,
                            "max_batch_ms":measurements.maximum_batch.as_millis()})
                    );
                    last_progress = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            stop.store(true, Ordering::Release);
            let reads = readers
                .into_iter()
                .map(|reader| reader.join().unwrap())
                .collect::<Vec<_>>();
            let writes = writer.join().unwrap();
            assert!(restarted, "recovery must cross a real cursor restart");
            assert_eq!(repairs, message_count);
            assert_eq!(
                final_retry_len,
                Some(0),
                "deferred repair debt did not drain"
            );
            assert!(
                reads.iter().all(|latencies| latencies.count >= 2),
                "competing readers did not run at least twice"
            );
            assert!(writes >= 2, "ordinary DB/archive writes did not run");
            (
                reads,
                writes,
                passes,
                repairs,
                final_retry_len.unwrap(),
                measurements,
            )
        });
    let reconciliation_elapsed_ms = started.elapsed().as_millis();
    eprintln!(
        "{}",
        serde_json::json!({"case":"gh374_archive_progress", "phase":"converged",
            "elapsed_ms":reconciliation_elapsed_ms, "passes":passes, "repairs":repairs,
            "final_retry_len":final_retry_len})
    );
    let verification_started = Instant::now();

    // Quiesce the server's background sync before releasing the real writer,
    // so observing that writer's release cannot race a new, legitimate lock.
    mcp_agent_mail_storage::stop_archive_index_sync(Duration::from_secs(5));
    git_writer.finish();
    assert!(!repo.path().join("index.lock").exists());
    let mut index = repo.index().unwrap();
    index.read(true).unwrap();
    assert_eq!(
        index.get_path(Path::new(pending_path), 0).unwrap().id,
        pending_oid
    );
    assert_eq!(
        index
            .get_path(Path::new("operator-later.txt"), 0)
            .unwrap()
            .id,
        later_oid
    );
    let head = repo.head().unwrap().peel_to_tree().unwrap();
    assert!(
        head.get_path(Path::new(pending_path)).is_err(),
        "repair published unrelated staged evidence"
    );
    assert!(
        head.get_path(Path::new("operator-later.txt")).is_err(),
        "repair published the concurrent Git writer's staging"
    );
    assert!(
        head.get_path(Path::new("projects/gh374/ordinary-progress.txt"))
            .is_ok()
    );
    assert_eq!(
        reconciliation_mailbox_evidence(&cx, &pool),
        original,
        "reconciliation changed source messages or receipts"
    );

    let recipients = vec!["GreenStone".to_string(), "RedFox".to_string()];
    for id in FIRST_ID..end_id {
        let created_ts = reconciliation_message_created_ts(id, large_mailbox);
        let created = chrono::DateTime::from_timestamp_micros(created_ts).unwrap();
        let paths = mcp_agent_mail_storage::message_paths(
            &archive,
            "BlueLake",
            &recipients,
            &created,
            &format!("handoff-{id}"),
            id,
        )
        .unwrap();
        let full = serde_json::json!({
            "id": id, "from": "BlueLake", "to": ["GreenStone"], "cc": [], "bcc": ["RedFox"],
            "subject": format!("handoff-{id}"), "created": mcp_agent_mail_db::micros_to_iso(created_ts),
            "thread_id": "recovery-thread", "topic": "handoffs", "project": "/gh374", "project_slug": "gh374",
            "importance": "high", "ack_required": true, "attachments": [], "reply_to": 700,
        });
        let mut inbox = full.clone();
        inbox["bcc"] = serde_json::json!([]);
        for (path, expected) in [(&paths.canonical, &full), (&paths.outbox, &full)]
            .into_iter()
            .chain(paths.inbox.iter().map(|path| (path, &inbox)))
        {
            let (message, body) = read_message_file(path).unwrap();
            assert_eq!(&message, expected);
            assert_eq!(body, reconciliation_message_body(id, large_mailbox));
            let entry = head
                .get_path(path.strip_prefix(&config.storage_root).unwrap())
                .unwrap();
            assert_eq!(
                repo.find_blob(entry.id()).unwrap().content(),
                std::fs::read(path).unwrap()
            );
        }
    }
    let mut canonical_count = 0;
    head.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        if root.starts_with("projects/gh374/messages/")
            && entry.name().is_ok_and(|name| name.ends_with(".md"))
        {
            canonical_count += 1;
        }
        git2::TreeWalkResult::Ok
    })
    .unwrap();
    assert_eq!(
        canonical_count, message_count,
        "extra canonical identities appeared"
    );
    assert_eq!(
        mcp_agent_mail_storage::list_agent_outbox(&archive, "BlueLake")
            .unwrap()
            .len(),
        message_count
    );
    for agent in &recipients {
        assert_eq!(
            list_agent_inbox(&archive, agent).unwrap().len(),
            message_count
        );
    }
    let settled_head = repo.head().unwrap().target();
    let mut repeated = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
    let live = AtomicBool::new(false);
    let repeat_limit = if large_mailbox { 4 * message_count } else { 12 };
    let repeat_deadline = Instant::now() + convergence_budget;
    let mut repeated_items = 0;
    let mut repeat_passes = 0;
    for pass in 0..repeat_limit {
        assert!(
            Instant::now() < repeat_deadline,
            "repeat verification timed out after {repeat_passes} passes / {repeated_items} items"
        );
        let report = reconcile_message_batch(&cx, &pool, &config, &mut repeated, &live).unwrap();
        assert_eq!((report.repaired, report.deferred), (0, 0), "{report:?}");
        assert_eq!(
            report.retry_len, 0,
            "repeat pass retained deferred repair debt"
        );
        assert!(!report.interrupted);
        repeated_items += report.unchanged;
        repeat_passes = pass + 1;
        if large_mailbox && repeated_items >= 2 * message_count && !report.budget_exhausted {
            break;
        }
    }
    assert!(
        !large_mailbox || repeated_items >= 2 * message_count,
        "repeat passes did not revisit enough history: {repeated_items}"
    );
    assert_eq!(
        repo.head().unwrap().target(),
        settled_head,
        "repeat passes created duplicate commits"
    );
    assert_eq!(reconciliation_mailbox_evidence(&cx, &pool), original);
    let conn = archive_test_outcome(fastmcp_core::block_on(pool.acquire(&cx)));
    assert_eq!(
        conn.query_sync("PRAGMA integrity_check", &[]).unwrap()[0]
            .get_as::<String>(0)
            .unwrap(),
        "ok"
    );
    assert!(
        conn.query_sync("SELECT last_active_ts FROM agents WHERE id=101", &[])
            .unwrap()[0]
            .get_as::<i64>(0)
            .unwrap()
            >= 3_000_000
    );
    println!(
        "{}",
        serde_json::json!({
            "case": "gh374_real_git_writer_and_inbox_readers", "large_mailbox": large_mailbox,
            "messages": message_count, "body_bytes": reconciliation_message_body(FIRST_ID, large_mailbox).len(),
            "database_bytes": database_bytes, "seed_batch_messages": SEED_BATCH_SIZE, "seed_elapsed_ms": seed_elapsed_ms,
            "repairs": repairs, "final_retry_len": final_retry_len,
            "initial_retry_len": initial_retry_len, "restored_retry_len": restored_retry_len,
            "convergence_budget_secs": convergence_budget.as_secs(),
            "passes": passes, "ordinary_writes": ordinary_writes, "elapsed_ms": started.elapsed().as_millis(),
            "reconciliation_elapsed_ms": reconciliation_elapsed_ms,
            "verification_elapsed_ms": verification_started.elapsed().as_millis(),
            "scanned": measurements.scanned, "unchanged": measurements.unchanged,
            "deferred_transient": measurements.deferred_transient,
            "peak_retry_len": measurements.peak_retry_len,
            "budget_exhausted_batches": measurements.budget_exhausted_batches,
            "live_git_lock_checks": measurements.live_git_lock_checks,
            "first_repair_ms": measurements.first_repair_ms,
            "max_batch_ms": measurements.maximum_batch.as_millis(),
            "repeat_passes": repeat_passes, "repeat_unchanged": repeated_items,
            "reader_counts": read_results.iter().map(|latencies| latencies.count).collect::<Vec<_>>(),
            "max_read_ms": read_results.iter().map(|latencies| latencies.maximum.as_millis()).max(),
            "reader_latency_ms": read_results.iter().map(|latencies| serde_json::json!({
                "p50":latencies.percentile_ms(50), "p95":latencies.percentile_ms(95),
                "p99":latencies.percentile_ms(99), "max":latencies.maximum.as_nanos().div_ceil(1_000_000),
            })).collect::<Vec<_>>(),
        })
    );
}
