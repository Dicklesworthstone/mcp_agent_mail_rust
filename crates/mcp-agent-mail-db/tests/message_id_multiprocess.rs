//! br-sa58k: message-id election must be durable and atomic across
//! independent OS processes.
//!
//! Two worker processes open the same mailbox database, register, and then
//! hold at one gate, so both have observed the same floor before either
//! allocates. Released together, each elects a run of ids through the
//! in-transaction election (`elect_message_id_in_tx` via `create_message`).
//! The parent asserts every committed id is distinct and that each id's row is
//! the message its worker wrote: the duplicate-canonical-id failure that
//! motivated the bead.
//!
//! A second test SIGKILLs a writer process in the middle of its election loop,
//! several times, and proves the election stays retryable: every acknowledged
//! id survives, no killed election advanced the durable allocator past the
//! committed rows, and the next writer elects exactly the following id.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use asupersync::{Cx, OutcomeError};
use mcp_agent_mail_db::create_pool;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::queries;

const TEST_NAME: &str = "two_processes_elect_distinct_message_ids_from_a_shared_floor";
const MESSAGES_PER_WORKER: usize = 20;
const PROJECT_KEY: &str = "/tmp/br-sa58k-worker";

fn worker_mode() -> Option<String> {
    std::env::var("MAGENTAROBIN_ID_WORKER_DB").ok()
}

#[test]
fn two_processes_elect_distinct_message_ids_from_a_shared_floor() {
    let Some(db_path) = worker_mode() else {
        run_parent();
        return;
    };
    run_worker(&db_path);
}

fn pool_config(db_path: &str) -> DbPoolConfig {
    DbPoolConfig {
        database_url: format!("sqlite:///{db_path}"),
        run_migrations: true,
        min_connections: 1,
        max_connections: 1,
        warmup_connections: 0,
        ..Default::default()
    }
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(180);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_worker(db_path: &str) {
    let name = std::env::var("MAGENTAROBIN_ID_WORKER_NAME").expect("worker name");
    let gates = std::path::PathBuf::from(
        std::env::var("MAGENTAROBIN_ID_WORKER_GATES").expect("worker gate directory"),
    );
    // The mailbox validates agent names as adjective+noun.
    let agent_name = if name == "A" {
        "BlueLake"
    } else {
        "GreenStone"
    };

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build worker runtime");
    runtime.block_on(async {
        let cx = Cx::current().expect("runtime installs worker context");
        // Worker A creates the schema alone; B opens only once A is set up, so
        // initialization never races (that path is br-wp4am's).
        if name == "B" {
            wait_for(&gates.join("ready-A"), "worker A setup");
        }
        let pool = create_pool(&pool_config(db_path)).expect("worker pool");
        let project = queries::ensure_project(&cx, &pool, PROJECT_KEY)
            .await
            .into_result()
            .expect("ensure project");
        let project_id = project.id.expect("project id");
        let sender = queries::register_agent(
            &cx,
            &pool,
            project_id,
            agent_name,
            "codex-cli",
            "test",
            None,
            None,
            None,
        )
        .await
        .into_result()
        .expect("register worker agent");
        let sender_id = sender.id.expect("sender id");

        // Both workers now hold an open pool over the same floor; neither has
        // allocated. The parent releases them together.
        std::fs::write(gates.join(format!("ready-{name}")), "").expect("raise ready gate");
        wait_for(&gates.join("go"), "the release gate");

        let mut ids = Vec::with_capacity(MESSAGES_PER_WORKER);
        let mut busy_retries = 0_u32;
        for index in 0..MESSAGES_PER_WORKER {
            let subject = format!("elected by {name} #{index}");
            let message = loop {
                match queries::create_message(
                    &cx, &pool, project_id, sender_id, &subject, "body", None, "normal", false,
                    "{}",
                )
                .await
                .into_result()
                {
                    Ok(message) => break message,
                    // Cross-process writers contend for one database; a busy
                    // retry is a client retry, and the rolled-back attempt's
                    // election rolls back with it.
                    Err(OutcomeError::Err(error))
                        if error.is_retryable() && busy_retries < 2_000 =>
                    {
                        busy_retries += 1;
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("worker {name} message {index}: {error:?}"),
                }
            };
            ids.push(message.id.expect("elected message id").to_string());
        }
        println!("worker {name}: {MESSAGES_PER_WORKER} ids, {busy_retries} busy retries");
        std::fs::write(gates.join(format!("result-{name}.txt")), ids.join("\n"))
            .expect("write worker result");
    });
}

fn spawn_worker(name: &'static str, db_path: &Path, gates: &Path) -> Child {
    let mut child = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            &mcp_agent_mail_test_helpers::libtest_path!(TEST_NAME),
            "--test-threads=1",
            "--nocapture",
        ])
        .env("MAGENTAROBIN_ID_WORKER_DB", db_path.display().to_string())
        .env("MAGENTAROBIN_ID_WORKER_NAME", name)
        .env("MAGENTAROBIN_ID_WORKER_GATES", gates.display().to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn id-election worker");
    let stdout = child.stdout.take().expect("worker stdout");
    let stderr = child.stderr.take().expect("worker stderr");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            println!("[{name}] {line}");
        }
    });
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[{name}] {line}");
        }
    });
    child
}

fn run_parent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("shared-floor.sqlite3");
    let gates = dir.path();
    let mut workers = [
        ("A", spawn_worker("A", &db_path, gates)),
        ("B", spawn_worker("B", &db_path, gates)),
    ];

    // Release only when BOTH workers are set up and parked, so both have seen
    // the same floor before either elects. A worker that dies early fails fast.
    let deadline = Instant::now() + Duration::from_secs(180);
    while !(gates.join("ready-A").exists() && gates.join("ready-B").exists()) {
        for (name, child) in &mut workers {
            if let Some(status) = child.try_wait().expect("poll worker") {
                panic!("worker {name} exited before the release gate: {status}");
            }
        }
        assert!(Instant::now() < deadline, "workers never both became ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(gates.join("go"), "").expect("release both workers");

    let mut elected: Vec<(&str, i64)> = Vec::new();
    for (name, child) in &mut workers {
        let status = child.wait().expect("wait worker");
        assert!(status.success(), "worker {name} exited unsuccessfully");
        let ids = std::fs::read_to_string(gates.join(format!("result-{name}.txt")))
            .expect("read worker result");
        let ids: Vec<i64> = ids
            .lines()
            .map(|line| line.parse().expect("worker id"))
            .collect();
        assert_eq!(ids.len(), MESSAGES_PER_WORKER, "worker {name} id count");
        elected.extend(ids.into_iter().map(|id| (*name, id)));
    }

    let distinct: HashSet<i64> = elected.iter().map(|(_, id)| *id).collect();
    assert_eq!(
        distinct.len(),
        elected.len(),
        "two OS processes elected the same message id: {elected:?}"
    );

    // Each reported id must be the committed row its own worker wrote, not a
    // row the other process published under the same id.
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build parent runtime");
    runtime.block_on(async {
        let cx = Cx::current().expect("runtime installs parent context");
        let pool = create_pool(&pool_config(&db_path.display().to_string())).expect("parent pool");
        for (name, id) in &elected {
            let row = queries::get_message(&cx, &pool, *id)
                .await
                .into_result()
                .unwrap_or_else(|error| panic!("message {id} from worker {name}: {error:?}"));
            assert!(
                row.subject.starts_with(&format!("elected by {name} #")),
                "id {id} reported by worker {name} holds {:?}",
                row.subject
            );
        }
    });
}

const CRASH_TEST_NAME: &str = "a_writer_killed_mid_election_leaves_the_election_retryable";
/// Committed messages the parent waits for before each SIGKILL; varied so the
/// kill lands at different points of the writer's loop.
const CRASH_KILL_AFTER: [usize; 3] = [3, 7, 12];

/// br-sa58k gap 3: a process killed mid-election never burns, reserves or
/// duplicates an id. The worker elects in a tight loop and reports each id
/// only after its transaction committed; the parent SIGKILLs it while the
/// next election is in flight, three times over one database.
#[test]
fn a_writer_killed_mid_election_leaves_the_election_retryable() {
    let Ok(db_path) = std::env::var("MAGENTAROBIN_CRASH_WORKER_DB") else {
        run_crash_parent();
        return;
    };
    run_crash_worker(&db_path);
}

fn run_crash_worker(db_path: &str) {
    let cycle = std::env::var("MAGENTAROBIN_CRASH_WORKER_CYCLE").expect("worker cycle");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build worker runtime");
    runtime.block_on(async {
        let cx = Cx::current().expect("runtime installs worker context");
        let pool = create_pool(&pool_config(db_path)).expect("worker pool");
        let project = queries::ensure_project(&cx, &pool, PROJECT_KEY)
            .await
            .into_result()
            .expect("ensure project");
        let project_id = project.id.expect("project id");
        let sender = queries::register_agent(
            &cx,
            &pool,
            project_id,
            "BlueLake",
            "codex-cli",
            "test",
            None,
            None,
            None,
        )
        .await
        .into_result()
        .expect("register worker agent");
        let sender_id = sender.id.expect("sender id");
        // Elect until killed. A line is printed only after its commit.
        for index in 0_u64.. {
            let subject = format!("crash cycle {cycle} #{index}");
            match queries::create_message(
                &cx, &pool, project_id, sender_id, &subject, "body", None, "normal", false, "{}",
            )
            .await
            .into_result()
            {
                Ok(message) => {
                    let id = message.id.expect("elected message id");
                    let mut stdout = std::io::stdout().lock();
                    writeln!(stdout, "committed {id} {subject}").expect("report commit");
                    stdout.flush().expect("flush commit report");
                }
                Err(OutcomeError::Err(error)) if error.is_retryable() => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("crash worker cycle {cycle} message {index}: {error:?}"),
            }
        }
    });
}

fn run_crash_parent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("crash-election.sqlite3");
    let mut acknowledged: Vec<(i64, String)> = Vec::new();
    for (cycle, kill_after) in CRASH_KILL_AFTER.into_iter().enumerate() {
        let mut child = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                &mcp_agent_mail_test_helpers::libtest_path!(CRASH_TEST_NAME),
                "--test-threads=1",
                "--nocapture",
            ])
            .env(
                "MAGENTAROBIN_CRASH_WORKER_DB",
                db_path.display().to_string(),
            )
            .env("MAGENTAROBIN_CRASH_WORKER_CYCLE", cycle.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn crash worker");
        let stdout = child.stdout.take().expect("worker stdout");
        let mut seen = 0;
        let mut killed = false;
        // After the kill the pipe still yields every line written before it.
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Some(report) = line.strip_prefix("committed ") else {
                continue;
            };
            let (id, subject) = report.split_once(' ').expect("id and subject");
            acknowledged.push((id.parse().expect("committed id"), subject.to_string()));
            seen += 1;
            if seen == kill_after && !killed {
                child.kill().expect("SIGKILL the writer mid-election");
                killed = true;
            }
        }
        let status = child.wait().expect("reap crash worker");
        assert!(
            killed && !status.success(),
            "cycle {cycle}: worker must be killed after {kill_after} commits, exited {status}"
        );
    }

    verify_after_crashes(&db_path, &acknowledged);
}

/// Recover the database the killed writers left and prove the election state.
fn verify_after_crashes(db_path: &Path, acknowledged: &[(i64, String)]) {
    let distinct: HashSet<i64> = acknowledged.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        distinct.len(),
        acknowledged.len(),
        "duplicate ids: {acknowledged:?}"
    );

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build parent runtime");
    runtime.block_on(async {
        let cx = Cx::current().expect("runtime installs parent context");
        let pool = create_pool(&pool_config(&db_path.display().to_string())).expect("parent pool");
        for (id, subject) in acknowledged {
            let row = queries::get_message(&cx, &pool, *id)
                .await
                .into_result()
                .unwrap_or_else(|error| panic!("acknowledged message {id} lost: {error:?}"));
            assert_eq!(&row.subject, subject, "id {id} holds another message");
        }

        let (max_id, durable_seq) = {
            let conn = pool.acquire(&cx).await.into_result().expect("acquire");
            let rows = conn
                .query_sync(
                    "SELECT COALESCE(MAX(id), 0), \
                     (SELECT seq FROM sqlite_sequence WHERE name = 'messages') FROM messages",
                    &[],
                )
                .expect("allocator state");
            let row = rows.first().expect("allocator row");
            (
                row.get_as::<i64>(0).expect("max id"),
                row.get_as::<i64>(1).expect("durable sequence"),
            )
        };
        assert!(
            max_id >= distinct.iter().copied().max().expect("acknowledged ids"),
            "committed rows end below an acknowledged id"
        );
        assert_eq!(
            durable_seq, max_id,
            "a killed election must roll back with its transaction, never advance the allocator"
        );

        let project = queries::ensure_project(&cx, &pool, PROJECT_KEY)
            .await
            .into_result()
            .expect("ensure project");
        let sender = queries::register_agent(
            &cx,
            &pool,
            project.id.expect("project id"),
            "BlueLake",
            "codex-cli",
            "test",
            None,
            None,
            None,
        )
        .await
        .into_result()
        .expect("re-register agent");
        let next = queries::create_message(
            &cx,
            &pool,
            project.id.expect("project id"),
            sender.id.expect("sender id"),
            "after the crashes",
            "body",
            None,
            "normal",
            false,
            "{}",
        )
        .await
        .into_result()
        .expect("election stays retryable after SIGKILL");
        assert_eq!(
            next.id,
            Some(max_id + 1),
            "the next election follows the last commit"
        );

        let integrity = pool.run_full_integrity_check().expect("integrity check");
        assert!(
            integrity.ok,
            "integrity after SIGKILLs: {:?}",
            integrity.details
        );
    });
}
