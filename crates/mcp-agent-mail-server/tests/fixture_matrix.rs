//! Multi-project fixture matrix with artifact-rich CI diagnostics (br-3vwi.10.4).
//!
//! Provides reusable seed datasets for multi-project, multi-agent scenarios.
//! Each scenario creates an isolated SQLite database with realistic entity
//! relationships and produces JSON artifact reports for CI regression debugging.
//!
//! # Scenarios
//!
//! - **Baseline**: Single project, 2 agents, 5 messages.
//! - **Multi-project**: 3 projects, 8 agents, cross-project contacts, 30+ messages.
//! - **Contention-heavy**: Many overlapping file reservations and agent links.
//! - **High-traffic**: Large mailbox with 500+ messages, threads, mixed importance.
//!
//! # Artifact output
//!
//! JSON reports under `tests/artifacts/fixtures/{timestamp}_{pid}/` containing
//! entity inventories, timing, and validation results.

#![forbid(unsafe_code)]
#![allow(
    clippy::too_many_arguments,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::unnecessary_cast,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::type_complexity
)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use mcp_agent_mail_db::sqlmodel::Value as SqlValue;

// ── Test environment ────────────────────────────────────────────────────

struct FixtureEnv {
    _tmp: tempfile::TempDir,
    db_path: PathBuf,
}

impl FixtureEnv {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("fixture_matrix.sqlite3");
        Self { _tmp: tmp, db_path }
    }

    fn conn(&self) -> mcp_agent_mail_db::DbConn {
        let conn = mcp_agent_mail_db::DbConn::open_file(self.db_path.display().to_string())
            .expect("open sqlite db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        conn
    }

    fn open_conn(&self) -> mcp_agent_mail_db::DbConn {
        mcp_agent_mail_db::DbConn::open_file(self.db_path.display().to_string())
            .expect("open sqlite db")
    }
}

// ── DB insertion helpers ────────────────────────────────────────────────

fn insert_project(conn: &mcp_agent_mail_db::DbConn, id: i64, slug: &str, human_key: &str) {
    conn.execute_sync(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
        &[
            SqlValue::BigInt(id),
            SqlValue::Text(slug.to_string()),
            SqlValue::Text(human_key.to_string()),
            SqlValue::BigInt(1_704_067_200_000_000), // 2024-01-01T00:00:00Z
        ],
    )
    .expect("insert project");
}

fn insert_agent(
    conn: &mcp_agent_mail_db::DbConn,
    id: i64,
    project_id: i64,
    name: &str,
    program: &str,
    model: &str,
) {
    conn.execute_sync(
        "INSERT INTO agents (\
            id, project_id, name, program, model, task_description, inception_ts, last_active_ts, \
            attachments_policy, contact_policy\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            SqlValue::BigInt(id),
            SqlValue::BigInt(project_id),
            SqlValue::Text(name.to_string()),
            SqlValue::Text(program.to_string()),
            SqlValue::Text(model.to_string()),
            SqlValue::Text(String::new()),
            SqlValue::BigInt(1_704_067_200_000_000),
            SqlValue::BigInt(1_704_067_200_000_000),
            SqlValue::Text("auto".to_string()),
            SqlValue::Text("auto".to_string()),
        ],
    )
    .expect("insert agent");
}

fn insert_message(
    conn: &mcp_agent_mail_db::DbConn,
    id: i64,
    project_id: i64,
    sender_id: i64,
    subject: &str,
    body: &str,
    thread_id: Option<&str>,
    importance: &str,
    ack_required: bool,
    ts_offset: i64,
) {
    let base_ts: i64 = 1_704_067_200_000_000; // 2024-01-01T00:00:00Z
    conn.execute_sync(
        "INSERT INTO messages (\
            id, project_id, sender_id, subject, body_md, importance, ack_required, \
            created_ts, thread_id\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            SqlValue::BigInt(id),
            SqlValue::BigInt(project_id),
            SqlValue::BigInt(sender_id),
            SqlValue::Text(subject.to_string()),
            SqlValue::Text(body.to_string()),
            SqlValue::Text(importance.to_string()),
            SqlValue::Bool(ack_required),
            SqlValue::BigInt(base_ts + ts_offset * 1_000_000),
            match thread_id {
                Some(t) => SqlValue::Text(t.to_string()),
                None => SqlValue::Null,
            },
        ],
    )
    .expect("insert message");
}

fn insert_recipient(conn: &mcp_agent_mail_db::DbConn, message_id: i64, agent_id: i64, kind: &str) {
    conn.execute_sync(
        "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)",
        &[
            SqlValue::BigInt(message_id),
            SqlValue::BigInt(agent_id),
            SqlValue::Text(kind.to_string()),
        ],
    )
    .expect("insert recipient");
}

fn insert_file_reservation(
    conn: &mcp_agent_mail_db::DbConn,
    id: i64,
    project_id: i64,
    agent_id: i64,
    path: &str,
    exclusive: bool,
    expires_offset: i64,
    released: bool,
) {
    let base_ts: i64 = 1_704_067_200_000_000;
    let released_ts = if released {
        SqlValue::BigInt(base_ts + expires_offset * 500_000)
    } else {
        SqlValue::Null
    };
    conn.execute_sync(
        "INSERT INTO file_reservations (\
            id, project_id, agent_id, path_pattern, exclusive, reason, \
            created_ts, expires_ts, released_ts\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            SqlValue::BigInt(id),
            SqlValue::BigInt(project_id),
            SqlValue::BigInt(agent_id),
            SqlValue::Text(path.to_string()),
            SqlValue::Bool(exclusive),
            SqlValue::Text("fixture".to_string()),
            SqlValue::BigInt(base_ts),
            SqlValue::BigInt(base_ts + expires_offset * 1_000_000),
            released_ts,
        ],
    )
    .expect("insert file reservation");
}

fn insert_agent_link(
    conn: &mcp_agent_mail_db::DbConn,
    id: i64,
    a_project_id: i64,
    a_agent_id: i64,
    b_project_id: i64,
    b_agent_id: i64,
    status: &str,
) {
    let base_ts: i64 = 1_704_067_200_000_000;
    conn.execute_sync(
        "INSERT INTO agent_links (\
            id, a_project_id, a_agent_id, b_project_id, b_agent_id, \
            status, reason, created_ts, updated_ts\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            SqlValue::BigInt(id),
            SqlValue::BigInt(a_project_id),
            SqlValue::BigInt(a_agent_id),
            SqlValue::BigInt(b_project_id),
            SqlValue::BigInt(b_agent_id),
            SqlValue::Text(status.to_string()),
            SqlValue::Text("fixture".to_string()),
            SqlValue::BigInt(base_ts),
            SqlValue::BigInt(base_ts),
        ],
    )
    .expect("insert agent link");
}

// ── Entity count query ──────────────────────────────────────────────────

fn count_table(conn: &mcp_agent_mail_db::DbConn, table: &str) -> i64 {
    let rows = conn
        .query_sync(&format!("SELECT COUNT(*) FROM {table}"), &[])
        .expect("count query");
    rows.first()
        .and_then(|r| r.get_by_name("COUNT(*)"))
        .and_then(|v| match v {
            SqlValue::BigInt(n) => Some(*n),
            SqlValue::Int(n) => Some(i64::from(*n)),
            _ => None,
        })
        .unwrap_or(0)
}

// ── Scenario inventory ──────────────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
struct EntityInventory {
    projects: i64,
    agents: i64,
    messages: i64,
    recipients: i64,
    file_reservations: i64,
    agent_links: i64,
}

impl EntityInventory {
    fn from_db(conn: &mcp_agent_mail_db::DbConn) -> Self {
        Self {
            projects: count_table(conn, "projects"),
            agents: count_table(conn, "agents"),
            messages: count_table(conn, "messages"),
            recipients: count_table(conn, "message_recipients"),
            file_reservations: count_table(conn, "file_reservations"),
            agent_links: count_table(conn, "agent_links"),
        }
    }
}

// ── Artifact report ─────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
struct FixtureReport {
    generated_at: String,
    scenario: String,
    seed_duration_ms: u64,
    query_duration_ms: u64,
    inventory: EntityInventory,
    validations: Vec<Validation>,
    verdict: String,
}

#[derive(Debug, serde::Serialize)]
struct Validation {
    check: String,
    expected: String,
    actual: String,
    passed: bool,
}

fn artifacts_dir() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root");
    root.join("tests/artifacts/fixtures")
}

fn write_report(scenario: &str, report: &FixtureReport) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ").to_string();
    let pid = std::process::id();
    let dir = artifacts_dir().join(format!("{ts}_{pid}"));
    std::fs::create_dir_all(&dir).expect("create artifacts dir");
    let path = dir.join(format!("{scenario}.json"));
    let json = serde_json::to_string_pretty(report).expect("serialize report");
    std::fs::write(&path, json).expect("write artifact");
    eprintln!("fixture artifact: {}", path.display());
}

// ── Scenario builders ───────────────────────────────────────────────────

/// Baseline: 1 project, 2 agents, 5 messages.
fn seed_baseline(conn: &mcp_agent_mail_db::DbConn) {
    insert_project(conn, 1, "baseline-proj", "/tmp/baseline");
    insert_agent(conn, 1, 1, "AlphaAgent", "claude-code", "claude-opus-4-6");
    insert_agent(conn, 2, 1, "BetaAgent", "codex", "gpt-5");

    for i in 1..=5 {
        let sender = if i % 2 == 0 { 1 } else { 2 };
        let recipient = if i % 2 == 0 { 2 } else { 1 };
        insert_message(
            conn,
            i,
            1,
            sender,
            &format!("Test message {i}"),
            &format!("Body of message {i}"),
            Some("baseline-thread"),
            "normal",
            i == 3, // One ack-required
            i * 60,
        );
        insert_recipient(conn, i, recipient, "to");
    }
}

/// Multi-project: 3 projects, 8 agents, cross-project contacts, 30+ messages.
fn seed_multi_project(conn: &mcp_agent_mail_db::DbConn) {
    // Projects
    insert_project(conn, 1, "frontend-app", "/data/projects/frontend");
    insert_project(conn, 2, "backend-api", "/data/projects/backend");
    insert_project(conn, 3, "shared-libs", "/data/projects/shared");

    // Agents per project
    let agents = [
        (1, 1, "RedFox", "claude-code", "claude-opus-4-6"),
        (2, 1, "BlueBear", "codex", "gpt-5"),
        (3, 1, "GreenOwl", "claude-code", "claude-sonnet-4-5"),
        (4, 2, "GoldEagle", "claude-code", "claude-opus-4-6"),
        (5, 2, "SilverWolf", "gemini", "gemini-ultra"),
        (6, 2, "CopperRobin", "codex", "gpt-5"),
        (7, 3, "IronHawk", "claude-code", "claude-opus-4-6"),
        (8, 3, "BronzeDeer", "claude-code", "claude-sonnet-4-5"),
    ];
    for (id, proj, name, prog, model) in agents {
        insert_agent(conn, id, proj, name, prog, model);
    }

    // Messages across projects with threads
    let threads = ["feature-auth", "bug-login", "refactor-db", "deploy-staging"];
    let mut msg_id = 1;
    for proj_id in 1..=3_i64 {
        let proj_agents: Vec<i64> = agents
            .iter()
            .filter(|(_, p, _, _, _)| *p == proj_id)
            .map(|(id, _, _, _, _)| *id)
            .collect();

        for (t_idx, thread) in threads.iter().enumerate() {
            for msg_offset in 0..3 {
                let sender_idx = (t_idx + msg_offset) % proj_agents.len();
                let sender = proj_agents[sender_idx];
                let recipient_idx = (sender_idx + 1) % proj_agents.len();
                let recipient = proj_agents[recipient_idx];

                let importance = if msg_offset == 0 { "high" } else { "normal" };
                let ack = msg_offset == 0;

                insert_message(
                    conn,
                    msg_id,
                    proj_id,
                    sender,
                    &format!("[{thread}] Update #{}", msg_offset + 1),
                    &format!("Progress update on {thread} from agent {sender}"),
                    Some(thread),
                    importance,
                    ack,
                    (msg_id * 30) as i64,
                );
                insert_recipient(conn, msg_id, recipient, "to");
                // CC another agent if available
                if proj_agents.len() > 2 {
                    let cc_idx = (recipient_idx + 1) % proj_agents.len();
                    if cc_idx != sender_idx {
                        insert_recipient(conn, msg_id, proj_agents[cc_idx], "cc");
                    }
                }
                msg_id += 1;
            }
        }
    }

    // Cross-project agent links (contacts)
    let mut link_id = 1;
    // Frontend RedFox ↔ Backend GoldEagle
    insert_agent_link(conn, link_id, 1, 1, 2, 4, "approved");
    link_id += 1;
    // Frontend BlueBear ↔ Shared IronHawk
    insert_agent_link(conn, link_id, 1, 2, 3, 7, "approved");
    link_id += 1;
    // Backend SilverWolf ↔ Shared BronzeDeer (pending)
    insert_agent_link(conn, link_id, 2, 5, 3, 8, "pending");
    let _ = link_id;
}

/// Contention-heavy: many overlapping file reservations.
fn seed_contention(conn: &mcp_agent_mail_db::DbConn) {
    insert_project(conn, 1, "contention-proj", "/data/projects/contention");

    for i in 1..=6 {
        insert_agent(
            conn,
            i,
            1,
            &format!("Agent{i}"),
            "claude-code",
            "claude-opus-4-6",
        );
    }

    // Create overlapping reservations
    let files = [
        "src/main.rs",
        "src/lib.rs",
        "src/config.rs",
        "src/models.rs",
        "src/api.rs",
        "Cargo.toml",
        "README.md",
        "tests/**",
    ];

    let mut res_id = 1;
    for (f_idx, file) in files.iter().enumerate() {
        // Multiple agents contend for same files
        for agent_offset in 0..3 {
            let agent_id = ((f_idx + agent_offset) % 6 + 1) as i64;
            let exclusive = agent_offset == 0;
            let released = agent_offset > 0;
            let expires_offset = ((f_idx + 1) * 3600) as i64;

            insert_file_reservation(
                conn,
                res_id,
                1,
                agent_id,
                file,
                exclusive,
                expires_offset,
                released,
            );
            res_id += 1;
        }
    }

    // Messages about contention
    for i in 1..=10 {
        let sender = ((i - 1) % 6 + 1) as i64;
        let recipient = (i % 6 + 1) as i64;
        insert_message(
            conn,
            i as i64,
            1,
            sender,
            &format!("[coord] File reservation conflict #{i}"),
            &format!(
                "Need to resolve contention on {} by Agent{sender}",
                files[(i - 1) % files.len()]
            ),
            Some("contention-coord"),
            if i <= 3 { "urgent" } else { "normal" },
            i <= 3,
            (i * 120) as i64,
        );
        insert_recipient(conn, i as i64, recipient, "to");
    }
}

/// High-traffic: 500+ messages, many threads, mixed importance.
fn seed_high_traffic(conn: &mcp_agent_mail_db::DbConn) {
    insert_project(conn, 1, "high-traffic-proj", "/data/projects/traffic");

    let agent_count = 10;
    for i in 1..=agent_count {
        let program = if i % 3 == 0 {
            "codex"
        } else if i % 3 == 1 {
            "claude-code"
        } else {
            "gemini"
        };
        let model = if i % 3 == 0 {
            "gpt-5"
        } else if i % 3 == 1 {
            "claude-opus-4-6"
        } else {
            "gemini-ultra"
        };
        insert_agent(conn, i, 1, &format!("Worker{i}"), program, model);
    }

    let thread_prefixes = [
        "build", "deploy", "review", "test", "release", "hotfix", "feature", "docs",
    ];
    let importances = ["normal", "normal", "normal", "high", "urgent"];

    let msg_count: i64 = 500;
    for i in 1..=msg_count {
        let sender = (i - 1) % agent_count + 1;
        let recipient = i % agent_count + 1;
        let thread_idx = ((i - 1) as usize) % thread_prefixes.len();
        let thread_batch = ((i - 1) as usize) / thread_prefixes.len();
        let thread_id = format!("{}-{}", thread_prefixes[thread_idx], thread_batch);
        let importance = importances[(i as usize) % importances.len()];
        let ack = i % 7 == 0;

        insert_message(
            conn,
            i,
            1,
            sender,
            &format!(
                "[{}] batch {} msg {i}",
                thread_prefixes[thread_idx], thread_batch
            ),
            &format!("Automated workload message {i} in thread {thread_id}"),
            Some(&thread_id),
            importance,
            ack,
            i * 5,
        );
        insert_recipient(conn, i, recipient, "to");

        // Add CC for urgent messages
        if importance == "urgent" {
            let cc = (i + 2) % agent_count + 1;
            if cc != sender && cc != recipient {
                insert_recipient(conn, i, cc, "cc");
            }
        }
    }

    // Add some file reservations
    for i in 1_i64..=20 {
        let agent_id = (i - 1) % agent_count + 1;
        insert_file_reservation(
            conn,
            i,
            1,
            agent_id,
            &format!("src/module_{}.rs", i % 10),
            i % 3 == 0,
            i * 1800,
            i > 15,
        );
    }
}

// ── Validation helpers ──────────────────────────────────────────────────

fn validate_count(validations: &mut Vec<Validation>, check: &str, expected: i64, actual: i64) {
    validations.push(Validation {
        check: check.to_string(),
        expected: expected.to_string(),
        actual: actual.to_string(),
        passed: actual == expected,
    });
}

fn validate_min(validations: &mut Vec<Validation>, check: &str, min: i64, actual: i64) {
    validations.push(Validation {
        check: check.to_string(),
        expected: format!(">= {min}"),
        actual: actual.to_string(),
        passed: actual >= min,
    });
}

// ── Scenario runner ─────────────────────────────────────────────────────

fn run_scenario<F: FnOnce(&mcp_agent_mail_db::DbConn)>(
    name: &str,
    seed_fn: F,
    validate_fn: impl FnOnce(&mcp_agent_mail_db::DbConn, &EntityInventory) -> Vec<Validation>,
) -> FixtureReport {
    let env = FixtureEnv::new();
    let conn = env.conn();

    let seed_start = Instant::now();
    seed_fn(&conn);
    let seed_duration = seed_start.elapsed();

    // Re-open connection for clean query state.
    drop(conn);
    let conn = env.open_conn();

    let query_start = Instant::now();
    let inventory = EntityInventory::from_db(&conn);
    let validations = validate_fn(&conn, &inventory);
    let query_duration = query_start.elapsed();

    let all_passed = validations.iter().all(|v| v.passed);

    let report = FixtureReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        scenario: name.to_string(),
        seed_duration_ms: seed_duration.as_millis() as u64,
        query_duration_ms: query_duration.as_millis() as u64,
        inventory,
        validations,
        verdict: if all_passed {
            "PASS".to_string()
        } else {
            "FAIL".to_string()
        },
    };

    write_report(name, &report);
    report
}

// ── Tests ───────────────────────────────────────────────────────────────

#[test]
fn fixture_baseline() {
    let report = run_scenario("baseline", seed_baseline, |_conn, inv| {
        let mut v = Vec::new();
        validate_count(&mut v, "projects", 1, inv.projects);
        validate_count(&mut v, "agents", 2, inv.agents);
        validate_count(&mut v, "messages", 5, inv.messages);
        validate_count(&mut v, "recipients", 5, inv.recipients);
        v
    });
    assert_eq!(report.verdict, "PASS", "baseline validations failed");
    assert_eq!(report.inventory.projects, 1);
    assert_eq!(report.inventory.agents, 2);
    assert_eq!(report.inventory.messages, 5);
}

#[test]
fn fixture_multi_project() {
    let report = run_scenario("multi_project", seed_multi_project, |_conn, inv| {
        let mut v = Vec::new();
        validate_count(&mut v, "projects", 3, inv.projects);
        validate_count(&mut v, "agents", 8, inv.agents);
        // 3 projects × 4 threads × 3 messages = 36
        validate_count(&mut v, "messages", 36, inv.messages);
        validate_min(&mut v, "recipients >= 36", 36, inv.recipients);
        validate_count(&mut v, "agent_links", 3, inv.agent_links);
        v
    });
    assert_eq!(report.verdict, "PASS", "multi_project validations failed");
    assert_eq!(report.inventory.projects, 3);
    assert_eq!(report.inventory.agents, 8);
    assert!(report.inventory.agent_links >= 3);
}

#[test]
fn fixture_contention() {
    let report = run_scenario("contention", seed_contention, |_conn, inv| {
        let mut v = Vec::new();
        validate_count(&mut v, "projects", 1, inv.projects);
        validate_count(&mut v, "agents", 6, inv.agents);
        validate_count(&mut v, "messages", 10, inv.messages);
        // 8 files × 3 agents = 24 reservations
        validate_count(&mut v, "file_reservations", 24, inv.file_reservations);
        v
    });
    assert_eq!(report.verdict, "PASS", "contention validations failed");
    assert_eq!(report.inventory.file_reservations, 24);
}

#[test]
fn fixture_high_traffic() {
    let report = run_scenario("high_traffic", seed_high_traffic, |_conn, inv| {
        let mut v = Vec::new();
        validate_count(&mut v, "projects", 1, inv.projects);
        validate_count(&mut v, "agents", 10, inv.agents);
        validate_count(&mut v, "messages", 500, inv.messages);
        validate_min(&mut v, "recipients >= 500", 500, inv.recipients);
        validate_count(&mut v, "file_reservations", 20, inv.file_reservations);
        v
    });
    assert_eq!(report.verdict, "PASS", "high_traffic validations failed");
    assert_eq!(report.inventory.messages, 500);
}

#[test]
fn fixture_seed_performance() {
    // Verify all scenarios seed within reasonable time budgets.
    let env = FixtureEnv::new();

    let scenarios: Vec<(&str, Box<dyn FnOnce(&mcp_agent_mail_db::DbConn)>, u64)> = vec![
        ("baseline", Box::new(seed_baseline), 100),
        ("multi_project", Box::new(seed_multi_project), 200),
        ("contention", Box::new(seed_contention), 200),
        ("high_traffic", Box::new(seed_high_traffic), 2000),
    ];

    let mut results = Vec::new();

    for (name, seed_fn, budget_ms) in scenarios {
        let conn = env.conn();
        // Reset DB for each scenario.
        drop(conn);
        let fresh_env = FixtureEnv::new();
        let conn = fresh_env.conn();

        let start = Instant::now();
        seed_fn(&conn);
        let elapsed_ms = start.elapsed().as_millis() as u64;

        let passed = elapsed_ms <= budget_ms;
        results.push((name, elapsed_ms, budget_ms, passed));

        eprintln!(
            "  seed({name}): {elapsed_ms}ms (budget: {budget_ms}ms) {}",
            if passed { "OK" } else { "EXCEEDED" }
        );
    }

    // Write summary artifact.
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ").to_string();
    let pid = std::process::id();
    let dir = artifacts_dir().join(format!("{ts}_{pid}"));
    std::fs::create_dir_all(&dir).expect("create artifacts dir");

    let summary: Vec<serde_json::Value> = results
        .iter()
        .map(|(name, elapsed, budget, passed)| {
            serde_json::json!({
                "scenario": name,
                "elapsed_ms": elapsed,
                "budget_ms": budget,
                "passed": passed,
            })
        })
        .collect();

    let report = serde_json::json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "test": "fixture_seed_performance",
        "results": summary,
        "all_passed": results.iter().all(|(_, _, _, p)| *p),
    });

    let path = dir.join("seed_performance.json");
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    eprintln!("perf artifact: {}", path.display());

    for (name, elapsed, budget, _) in &results {
        assert!(
            elapsed <= budget,
            "seed({name}) took {elapsed}ms, budget is {budget}ms"
        );
    }
}

/// Matrix test: run all scenarios and produce a combined report.
#[test]
fn fixture_matrix_combined_report() {
    let scenarios: Vec<(&str, Box<dyn FnOnce(&mcp_agent_mail_db::DbConn)>)> = vec![
        ("baseline", Box::new(seed_baseline)),
        ("multi_project", Box::new(seed_multi_project)),
        ("contention", Box::new(seed_contention)),
        ("high_traffic", Box::new(seed_high_traffic)),
    ];

    let mut matrix = Vec::new();

    for (name, seed_fn) in scenarios {
        let env = FixtureEnv::new();
        let conn = env.conn();

        let start = Instant::now();
        seed_fn(&conn);
        let seed_ms = start.elapsed().as_millis() as u64;

        drop(conn);
        let conn = env.open_conn();
        let inventory = EntityInventory::from_db(&conn);

        matrix.push(serde_json::json!({
            "scenario": name,
            "seed_ms": seed_ms,
            "inventory": {
                "projects": inventory.projects,
                "agents": inventory.agents,
                "messages": inventory.messages,
                "recipients": inventory.recipients,
                "file_reservations": inventory.file_reservations,
                "agent_links": inventory.agent_links,
            },
        }));
    }

    // Write combined matrix artifact.
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ").to_string();
    let pid = std::process::id();
    let dir = artifacts_dir().join(format!("{ts}_{pid}"));
    std::fs::create_dir_all(&dir).expect("create artifacts dir");

    let report = serde_json::json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "agent": "GoldStream",
        "bead": "br-3vwi.10.4",
        "matrix": matrix,
    });

    let path = dir.join("matrix.json");
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    eprintln!("matrix artifact: {}", path.display());

    // Verify all scenarios produced data.
    assert_eq!(matrix.len(), 4, "expected 4 scenario results");
}
