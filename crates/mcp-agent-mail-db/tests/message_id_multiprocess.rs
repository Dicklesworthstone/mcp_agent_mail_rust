//! br-sa58k: message-id election must be durable and atomic across
//! independent OS processes.
//!
//! Two worker processes attach to the same mailbox database, observe the same
//! starting floor, and elect ids concurrently through the in-transaction
//! election (`elect_message_id_in_tx` via `create_message`). The parent
//! asserts the committed ids are distinct and that the canonical archive
//! filenames derived from them never collide — the duplicate-canonical-file
//! failure mode that motivated the bead.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use asupersync::Cx;
use mcp_agent_mail_db::create_pool;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::queries;

const TEST_NAME: &str = "two_processes_elect_distinct_message_ids_from_a_shared_floor";

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

fn wait_for_go(go_gate: &str, name: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    while !std::path::Path::new(go_gate).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "worker {name} never observed the go gate"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
fn run_worker(db_path: &str) {
    let name = std::env::var("MAGENTAROBIN_ID_WORKER_NAME").unwrap_or_else(|_| "A".to_string());
    let gate = std::env::var("MAGENTAROBIN_ID_WORKER_GATE").expect("worker gate path");
    let storage_root = std::path::PathBuf::from(
        std::env::var("MAGENTAROBIN_ID_WORKER_STORAGE").unwrap_or_default(),
    );
    let go_gate = format!("{gate}.go");
    // The mailbox validates agent names as adjective+noun; pick two valid
    // deterministic identities.
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
        let cfg = DbPoolConfig {
            database_url: format!("sqlite:///{db_path}"),
            run_migrations: true,
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };

        // Worker A initializes the mailbox alone (schema, project, its own
        // agent) and raises the gate, then waits for the release. Worker B
        // defers even its pool open until the release, so initialization and
        // registration never race.
        let (pool, project_id, sender_id) = if name == "A" {
            let pool = create_pool(&cfg).expect("initiator pool");
            let project = queries::ensure_project(&cx, &pool, "/tmp/br-sa58k-worker")
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
            .expect("register initiator agent");
            std::fs::write(&gate, "ready").expect("raise start gate");
            let sender_id = sender.id.expect("sender id");
            (pool, project_id, sender_id)
        } else {
            wait_for_go(&go_gate, &name);
            let pool = create_pool(&cfg).expect("follower pool");
            let project = queries::ensure_project(&cx, &pool, "/tmp/br-sa58k-worker")
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
            .expect("register follower agent");
            let sender_id = sender.id.expect("sender id");
            (pool, project_id, sender_id)
        };
        if name == "A" {
            wait_for_go(&go_gate, &name);
        }

        let message = queries::create_message(
            &cx,
            &pool,
            project_id,
            sender_id,
            &format!("elected by {name}"),
            "body",
            None,
            "normal",
            false,
            "{}",
        )
        .await
        .into_result()
        .expect("worker message creation");
        let id = message.id.expect("elected message id");

        // Project the elected id into a canonical-shaped archive filename so
        // the parent can prove filename-level distinctness.
        let canonical = storage_root
            .join("projects/p/messages/2026/07")
            .join(format!("{id:06}__elected.md"));
        if let Some(parent) = canonical.parent() {
            std::fs::create_dir_all(parent).expect("create canonical directory");
        }
        std::fs::write(&canonical, format!("---json\n{{\"id\": {id}}}\n---\n"))
            .expect("write canonical marker");

        std::fs::write(
            storage_root
                .parent()
                .expect("storage parent")
                .join(format!("result-{name}.txt")),
            format!("{id}\n"),
        )
        .expect("write worker result");
    });
}

fn run_parent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("shared-floor.sqlite3");
    let storage_root = dir.path().join("storage");
    std::fs::create_dir_all(&storage_root).expect("create storage root");
    let gate = dir.path().join("start.gate");
    let go_gate = dir.path().join("start.gate.go");
    let exe = std::env::current_exe().expect("current test executable");

    let spawn_worker = |name: &'static str| {
        let mut child = Command::new(&exe)
            .args(["--exact", TEST_NAME, "--test-threads=1", "--nocapture"])
            .env("MAGENTAROBIN_ID_WORKER_DB", db_path.display().to_string())
            .env("MAGENTAROBIN_ID_WORKER_NAME", name)
            .env("MAGENTAROBIN_ID_WORKER_GATE", gate.display().to_string())
            .env(
                "MAGENTAROBIN_ID_WORKER_STORAGE",
                storage_root.display().to_string(),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn id-election worker");
        let stdout = child.stdout.take().expect("worker stdout");
        let stderr = child.stderr.take().expect("worker stderr");
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                println!("[{name}] {line}");
            }
        });
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("[{name}] {line}");
            }
        });
        child
    };

    // Worker A initializes the mailbox schema and raises the start gate; both
    // workers then block until the parent atomically renames the gate, so
    // they race the same durable floor as simultaneously as the filesystem
    // allows.
    let mut worker_a = spawn_worker("A");
    let mut worker_b = spawn_worker("B");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    while !gate.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "worker A never raised the start gate"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::fs::rename(&gate, &go_gate).expect("release the start gate");

    let status_a = worker_a.wait().expect("wait worker A");
    let status_b = worker_b.wait().expect("wait worker B");
    assert!(status_a.success(), "worker A exited unsuccessfully");
    assert!(status_b.success(), "worker B exited unsuccessfully");

    let read_result = |name: &str| {
        let path = dir.path().join(format!("result-{name}.txt"));
        std::fs::read_to_string(path).expect("read worker result")
    };
    let id_a: i64 = read_result("A").trim().parse().expect("worker A id");
    let id_b: i64 = read_result("B").trim().parse().expect("worker B id");

    assert_ne!(
        id_a, id_b,
        "two OS processes elected the same canonical message id from one floor"
    );

    // Canonical archive filenames derive from the id; prove they are distinct
    // at the filename level, not just the integer level.
    let file_a = storage_root.join(format!("projects/p/messages/2026/07/{id_a:06}__elected.md"));
    let file_b = storage_root.join(format!("projects/p/messages/2026/07/{id_b:06}__elected.md"));
    assert!(
        file_a.exists(),
        "worker A canonical file missing: {file_a:?}"
    );
    assert!(
        file_b.exists(),
        "worker B canonical file missing: {file_b:?}"
    );
    assert_ne!(file_a, file_b, "canonical filenames collided");
}
