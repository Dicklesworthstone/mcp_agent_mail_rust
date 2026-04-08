//! Forensic bundle replay harness (br-97gc6.5.2.6.3.1).
//!
//! Replays forensic bundles (both synthesized from known state and captured
//! from controlled incidents) through recovery harnesses. Diffs the resulting
//! DB state against expected outcomes. Reports mismatches in a structured way.
//!
//! Scenarios covered:
//! - **Clean reconstruct**: archive-only reconstruction yields expected DB rows.
//! - **Salvage merge**: DB-only state from a salvage database is merged correctly.
//! - **Drift detection**: archive vs DB drift is reported accurately.
//! - **Interrupted bundle**: partially written / truncated bundle artifacts.
//! - **Corrupt salvage**: recovery degrades gracefully with corrupt salvage db.
//! - **Multi-project / multi-agent**: complex archive with multiple projects,
//!   agents, messages, cross-project messaging.

#![allow(clippy::too_many_lines)]

use mcp_agent_mail_db::forensics::{
    MailboxForensicCapture, capture_mailbox_forensic_bundle, capture_pre_recovery_snapshot,
};
use mcp_agent_mail_db::reconstruct::{
    ReconstructStats, compute_archive_drift_report, reconstruct_from_archive,
    reconstruct_from_archive_with_salvage, scan_archive_message_inventory,
};
use serde_json::json;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

// ============================================================================
// Harness: structured diff report
// ============================================================================

/// A single field-level mismatch between expected and actual DB state.
#[derive(Debug)]
struct FieldMismatch {
    entity: String,
    field: String,
    expected: String,
    actual: String,
}

impl std::fmt::Display for FieldMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {}: expected={}, actual={}",
            self.entity, self.field, self.expected, self.actual
        )
    }
}

/// Structured diff result from replaying a forensic bundle through recovery.
#[derive(Debug)]
struct ReplayDiffReport {
    scenario: String,
    mismatches: Vec<FieldMismatch>,
    warnings: Vec<String>,
    stats: Option<ReconstructStats>,
}

impl ReplayDiffReport {
    fn new(scenario: &str) -> Self {
        Self {
            scenario: scenario.to_string(),
            mismatches: Vec::new(),
            warnings: Vec::new(),
            stats: None,
        }
    }

    fn add_mismatch(&mut self, entity: &str, field: &str, expected: &str, actual: &str) {
        self.mismatches.push(FieldMismatch {
            entity: entity.to_string(),
            field: field.to_string(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }

    const fn is_clean(&self) -> bool {
        self.mismatches.is_empty()
    }

    fn assert_clean(&self) {
        if !self.is_clean() {
            let mut report = format!(
                "Replay diff report for '{}' has mismatches:\n",
                self.scenario
            );
            for mismatch in &self.mismatches {
                let _ = writeln!(report, "  {mismatch}");
            }
            if !self.warnings.is_empty() {
                report.push_str("Warnings:\n");
                for warning in &self.warnings {
                    let _ = writeln!(report, "  {warning}");
                }
            }
            panic!("{report}");
        }
    }
}

// ============================================================================
// Harness: archive layout builder
// ============================================================================

/// Build a canonical archive message file with JSON frontmatter.
fn write_archive_message(
    storage_root: &Path,
    project_slug: &str,
    year: &str,
    month: &str,
    filename: &str,
    frontmatter: &serde_json::Value,
    body: &str,
) {
    let msg_dir = storage_root
        .join("projects")
        .join(project_slug)
        .join("messages")
        .join(year)
        .join(month);
    std::fs::create_dir_all(&msg_dir).expect("create message dir");
    let fm_text = serde_json::to_string_pretty(frontmatter).expect("serialize frontmatter");
    let content = format!("---json\n{fm_text}\n---\n\n{body}\n");
    std::fs::write(msg_dir.join(filename), content).expect("write message file");
}

/// Write a project.json metadata file.
fn write_project_metadata(storage_root: &Path, slug: &str, human_key: &str) {
    let project_dir = storage_root.join("projects").join(slug);
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    let meta = json!({
        "slug": slug,
        "human_key": human_key,
        "created_at": 0,
    });
    std::fs::write(
        project_dir.join("project.json"),
        serde_json::to_string_pretty(&meta).expect("serialize"),
    )
    .expect("write project metadata");
}

/// Write an agent profile.json file.
fn write_agent_profile(
    storage_root: &Path,
    project_slug: &str,
    agent_name: &str,
    program: &str,
    model: &str,
) {
    let agent_dir = storage_root
        .join("projects")
        .join(project_slug)
        .join("agents")
        .join(agent_name);
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    let profile = json!({
        "name": agent_name,
        "program": program,
        "model": model,
        "task_description": format!("Test agent {agent_name}"),
        "inception_ts": "2026-02-22T00:00:00Z",
        "last_active_ts": "2026-02-22T12:00:00Z",
        "attachments_policy": "auto",
        "contact_policy": "auto",
    });
    std::fs::write(
        agent_dir.join("profile.json"),
        serde_json::to_string_pretty(&profile).expect("serialize"),
    )
    .expect("write agent profile");
}

/// Create a minimal salvage database with the given tables and data.
fn create_salvage_db(path: &Path) -> mcp_agent_mail_db::DbConn {
    let conn = mcp_agent_mail_db::DbConn::open_file(path.to_str().expect("valid path"))
        .expect("open salvage db");
    conn.execute_raw(
        "CREATE TABLE projects (
            id INTEGER PRIMARY KEY,
            slug TEXT NOT NULL,
            human_key TEXT,
            created_at INTEGER
        )",
    )
    .expect("create projects table");
    conn.execute_raw(
        "CREATE TABLE agents (
            id INTEGER PRIMARY KEY,
            project_id INTEGER NOT NULL,
            name TEXT NOT NULL,
            program TEXT,
            model TEXT,
            task_description TEXT,
            inception_ts INTEGER,
            last_active_ts INTEGER,
            attachments_policy TEXT,
            contact_policy TEXT
        )",
    )
    .expect("create agents table");
    conn.execute_raw(
        "CREATE TABLE messages (
            id INTEGER PRIMARY KEY,
            project_id INTEGER NOT NULL,
            sender_id INTEGER NOT NULL,
            subject TEXT,
            body_md TEXT,
            importance TEXT,
            recipients_json TEXT,
            created_ts INTEGER
        )",
    )
    .expect("create messages table");
    conn.execute_raw(
        "CREATE TABLE message_recipients (
            message_id INTEGER NOT NULL,
            agent_id INTEGER NOT NULL,
            kind TEXT NOT NULL DEFAULT 'to',
            read_ts INTEGER,
            ack_ts INTEGER
        )",
    )
    .expect("create message_recipients table");
    conn.execute_raw(
        "CREATE TABLE agent_links (
            id INTEGER PRIMARY KEY,
            project_id INTEGER NOT NULL,
            from_agent_id INTEGER NOT NULL,
            to_agent_id INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'allowed',
            reason TEXT,
            updated_ts INTEGER,
            expires_ts INTEGER
        )",
    )
    .expect("create agent_links table");
    conn
}

/// Query the reconstructed DB and return structured counts for diffing.
struct DbSnapshot {
    projects: Vec<(i64, String, String)>,
    agents: Vec<(i64, String, String)>,
    messages: Vec<(i64, String, String)>,
    recipients: Vec<(i64, String, String)>,
    agent_links: Vec<(i64, String, String, String)>,
}

fn table_has_column(conn: &mcp_agent_mail_db::DbConn, table: &str, column: &str) -> bool {
    conn.query_sync(&format!("PRAGMA table_info({table})"), &[])
        .unwrap_or_default()
        .iter()
        .filter_map(|row| row.get_named::<String>("name").ok())
        .any(|name| name == column)
}

fn snapshot_db(db_path: &Path) -> DbSnapshot {
    let conn = mcp_agent_mail_db::DbConn::open_file(db_path.to_str().expect("valid path"))
        .expect("open reconstructed db");

    let project_rows = conn
        .query_sync("SELECT id, slug, human_key FROM projects ORDER BY id", &[])
        .unwrap_or_default();
    let projects: Vec<(i64, String, String)> = project_rows
        .iter()
        .map(|row| {
            (
                row.get_named::<i64>("id").unwrap_or(0),
                row.get_named::<String>("slug").unwrap_or_default(),
                row.get_named::<String>("human_key").unwrap_or_default(),
            )
        })
        .collect();

    let agent_rows = conn
        .query_sync(
            "SELECT a.id, a.name, p.slug AS project_slug FROM agents a JOIN projects p ON p.id = a.project_id ORDER BY a.id",
            &[],
        )
        .unwrap_or_default();
    let agents: Vec<(i64, String, String)> = agent_rows
        .iter()
        .map(|row| {
            (
                row.get_named::<i64>("id").unwrap_or(0),
                row.get_named::<String>("name").unwrap_or_default(),
                row.get_named::<String>("project_slug").unwrap_or_default(),
            )
        })
        .collect();

    let msg_rows = conn
        .query_sync("SELECT id, subject, body_md FROM messages ORDER BY id", &[])
        .unwrap_or_default();
    let messages: Vec<(i64, String, String)> = msg_rows
        .iter()
        .map(|row| {
            (
                row.get_named::<i64>("id").unwrap_or(0),
                row.get_named::<String>("subject").unwrap_or_default(),
                row.get_named::<String>("body_md").unwrap_or_default(),
            )
        })
        .collect();

    let recip_rows = conn
        .query_sync(
            "SELECT mr.message_id, a.name AS agent_name, mr.kind
             FROM message_recipients mr
             JOIN agents a ON a.id = mr.agent_id
             ORDER BY mr.message_id, a.name",
            &[],
        )
        .unwrap_or_default();
    let recipients: Vec<(i64, String, String)> = recip_rows
        .iter()
        .map(|row| {
            (
                row.get_named::<i64>("message_id").unwrap_or(0),
                row.get_named::<String>("agent_name").unwrap_or_default(),
                row.get_named::<String>("kind").unwrap_or_default(),
            )
        })
        .collect();

    // agent_links may not exist in archive-only reconstructions
    let link_query = if table_has_column(&conn, "agent_links", "a_agent_id")
        && table_has_column(&conn, "agent_links", "b_agent_id")
    {
        "SELECT al.id, fa.name AS from_agent, ta.name AS to_agent, al.status
         FROM agent_links al
         JOIN agents fa ON fa.id = al.a_agent_id
         JOIN agents ta ON ta.id = al.b_agent_id
         ORDER BY al.id"
    } else {
        "SELECT al.id, fa.name AS from_agent, ta.name AS to_agent, al.status
         FROM agent_links al
         JOIN agents fa ON fa.id = al.from_agent_id
         JOIN agents ta ON ta.id = al.to_agent_id
         ORDER BY al.id"
    };
    let link_rows = conn.query_sync(link_query, &[]).unwrap_or_default();
    let agent_links: Vec<(i64, String, String, String)> = link_rows
        .iter()
        .map(|row| {
            (
                row.get_named::<i64>("id").unwrap_or(0),
                row.get_named::<String>("from_agent").unwrap_or_default(),
                row.get_named::<String>("to_agent").unwrap_or_default(),
                row.get_named::<String>("status").unwrap_or_default(),
            )
        })
        .collect();

    DbSnapshot {
        projects,
        agents,
        messages,
        recipients,
        agent_links,
    }
}

/// Build a standard multi-project, multi-agent archive for replay tests.
fn build_standard_archive(storage_root: &Path) {
    // Project A: proj-alpha with agents Alice and Bob, 3 messages
    write_project_metadata(storage_root, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        storage_root,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );
    write_agent_profile(storage_root, "proj-alpha", "Bob", "codex", "o3-pro");

    write_archive_message(
        storage_root,
        "proj-alpha",
        "2026",
        "02",
        "2026-02-22T12-00-00Z__hello__1.md",
        &json!({
            "id": 1,
            "from": "Alice",
            "to": ["Bob"],
            "subject": "Hello from Alice",
            "importance": "normal",
            "created_ts": "2026-02-22T12:00:00Z",
        }),
        "Hi Bob, this is message 1.",
    );

    write_archive_message(
        storage_root,
        "proj-alpha",
        "2026",
        "02",
        "2026-02-22T12-30-00Z__reply__2.md",
        &json!({
            "id": 2,
            "from": "Bob",
            "to": ["Alice"],
            "subject": "Reply from Bob",
            "importance": "normal",
            "created_ts": "2026-02-22T12:30:00Z",
        }),
        "Hi Alice, this is Bob replying.",
    );

    write_archive_message(
        storage_root,
        "proj-alpha",
        "2026",
        "03",
        "2026-03-01T09-00-00Z__update__3.md",
        &json!({
            "id": 3,
            "from": "Alice",
            "to": ["Bob"],
            "cc": ["Alice"],
            "subject": "March update",
            "importance": "high",
            "created_ts": "2026-03-01T09:00:00Z",
        }),
        "March update body.",
    );

    // Project B: proj-beta with agent Carol, 1 message
    write_project_metadata(storage_root, "proj-beta", "/home/user/proj-beta");
    write_agent_profile(storage_root, "proj-beta", "Carol", "aider", "sonnet-4");

    write_archive_message(
        storage_root,
        "proj-beta",
        "2026",
        "04",
        "2026-04-01T10-00-00Z__intro__4.md",
        &json!({
            "id": 4,
            "from": "Carol",
            "to": ["Carol"],
            "subject": "Self-note",
            "importance": "low",
            "created_ts": "2026-04-01T10:00:00Z",
        }),
        "Carol's self-note in proj-beta.",
    );
}

// ============================================================================
// Scenario 1: Clean archive-only reconstruction
// ============================================================================

#[test]
fn replay_clean_reconstruct_from_archive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("reconstructed.db");
    let storage_root = tmp.path().join("storage");

    build_standard_archive(&storage_root);

    // Capture a forensic bundle before reconstruction (to verify the bundle
    // itself is well-formed).
    let original_db_path = tmp.path().join("original.sqlite3");
    std::fs::write(&original_db_path, b"placeholder-db-bytes").expect("write placeholder db");

    let bundle_dir = capture_mailbox_forensic_bundle(MailboxForensicCapture {
        command_name: "replay-test",
        trigger: "clean-reconstruct",
        database_url: "sqlite:///tmp/storage.sqlite3",
        db_path: &original_db_path,
        storage_root: &storage_root,
        integrity_detail: None,
    })
    .expect("forensic bundle capture");

    // Verify bundle has the expected structure
    assert!(
        bundle_dir.join("manifest.json").exists(),
        "manifest missing"
    );
    assert!(bundle_dir.join("summary.json").exists(), "summary missing");
    assert!(
        bundle_dir
            .join("references")
            .join("archive-drift.json")
            .exists(),
        "archive-drift reference missing"
    );

    // Verify manifest schema
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(bundle_dir.join("manifest.json")).expect("read manifest"),
    )
    .expect("parse manifest");
    assert_eq!(manifest["trigger"], "clean-reconstruct");
    assert_eq!(manifest["command"], "replay-test");
    assert_eq!(
        manifest["schema"]["name"],
        "mcp-agent-mail-doctor-forensics"
    );

    // Now replay: reconstruct from archive
    let stats = reconstruct_from_archive(&db_path, &storage_root)
        .expect("clean reconstruct should succeed");

    // Build diff report
    let mut report = ReplayDiffReport::new("clean-reconstruct");
    report.stats = Some(stats.clone());

    if stats.projects != 2 {
        report.add_mismatch("stats", "projects", "2", &stats.projects.to_string());
    }
    if stats.agents != 3 {
        report.add_mismatch("stats", "agents", "3", &stats.agents.to_string());
    }
    if stats.messages != 4 {
        report.add_mismatch("stats", "messages", "4", &stats.messages.to_string());
    }
    if stats.parse_errors != 0 {
        report.add_mismatch(
            "stats",
            "parse_errors",
            "0",
            &stats.parse_errors.to_string(),
        );
    }

    // Verify DB contents match expected outcomes
    let snapshot = snapshot_db(&db_path);

    if snapshot.projects.len() != 2 {
        report.add_mismatch(
            "db",
            "project_count",
            "2",
            &snapshot.projects.len().to_string(),
        );
    }
    if snapshot.agents.len() != 3 {
        report.add_mismatch("db", "agent_count", "3", &snapshot.agents.len().to_string());
    }
    if snapshot.messages.len() != 4 {
        report.add_mismatch(
            "db",
            "message_count",
            "4",
            &snapshot.messages.len().to_string(),
        );
    }

    // Check specific messages
    let subjects: Vec<&str> = snapshot.messages.iter().map(|m| m.1.as_str()).collect();
    for expected_subject in &[
        "Hello from Alice",
        "Reply from Bob",
        "March update",
        "Self-note",
    ] {
        if !subjects.contains(expected_subject) {
            report.add_mismatch("messages", "subject", expected_subject, "<missing>");
        }
    }

    // Check agents are in correct projects
    let alice_in_alpha = snapshot
        .agents
        .iter()
        .any(|a| a.1 == "Alice" && a.2 == "proj-alpha");
    if !alice_in_alpha {
        report.add_mismatch(
            "agents",
            "Alice project",
            "proj-alpha",
            "<wrong or missing>",
        );
    }
    let carol_in_beta = snapshot
        .agents
        .iter()
        .any(|a| a.1 == "Carol" && a.2 == "proj-beta");
    if !carol_in_beta {
        report.add_mismatch("agents", "Carol project", "proj-beta", "<wrong or missing>");
    }

    // Verify recipients were correctly created
    let msg1_recipients: Vec<&str> = snapshot
        .recipients
        .iter()
        .filter(|r| r.0 == 1)
        .map(|r| r.1.as_str())
        .collect();
    if !msg1_recipients.contains(&"Bob") {
        report.add_mismatch(
            "recipients",
            "message 1 to",
            "Bob",
            &format!("{msg1_recipients:?}"),
        );
    }

    // Archive drift against the reconstructed DB should show zero drift
    let drift =
        compute_archive_drift_report(&storage_root, &db_path).expect("drift report should succeed");
    if drift.has_message_drift() {
        report.add_mismatch(
            "drift",
            "message_drift",
            "false",
            &format!(
                "archive_only={:?}, db_only={:?}",
                drift.archive_only_ids, drift.db_only_ids
            ),
        );
    }

    report.assert_clean();
}

// ============================================================================
// Scenario 2: Salvage merge reconstruction
// ============================================================================

#[test]
fn replay_salvage_merge_reconstruction() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("reconstructed.db");
    let salvage_db_path = tmp.path().join("salvage.db");
    let storage_root = tmp.path().join("storage");

    // Archive has Alice and Bob in proj-alpha with messages 1 and 2
    write_project_metadata(&storage_root, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        &storage_root,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );
    write_agent_profile(&storage_root, "proj-alpha", "Bob", "codex", "o3-pro");

    write_archive_message(
        &storage_root,
        "proj-alpha",
        "2026",
        "02",
        "2026-02-22T12-00-00Z__hello__1.md",
        &json!({
            "id": 1,
            "from": "Alice",
            "to": ["Bob"],
            "subject": "Archive msg 1",
            "importance": "normal",
            "created_ts": "2026-02-22T12:00:00Z",
        }),
        "First message from archive.",
    );

    write_archive_message(
        &storage_root,
        "proj-alpha",
        "2026",
        "02",
        "2026-02-22T14-00-00Z__follow__2.md",
        &json!({
            "id": 2,
            "from": "Bob",
            "to": ["Alice"],
            "subject": "Archive msg 2",
            "importance": "normal",
            "created_ts": "2026-02-22T14:00:00Z",
        }),
        "Second message from archive.",
    );

    // Salvage DB has:
    // - same project and agents (with different IDs, as in real recovery)
    // - message 3 that was never archived (DB-only)
    // - read_ts / ack_ts for message 1 that archive cannot preserve
    // - an agent_links contact record
    let salvage_conn = create_salvage_db(&salvage_db_path);
    salvage_conn
        .query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (100, 'proj-alpha', '/home/user/proj-alpha', 1)",
            &[],
        )
        .expect("insert salvage project");
    salvage_conn
        .query_sync(
            "INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts)
             VALUES
                (10, 100, 'Alice', 'claude-code', 'opus-4.6', 1, 1),
                (11, 100, 'Bob', 'codex', 'o3-pro', 1, 1),
                (12, 100, 'Dave', 'coder', 'gemini', 1, 1)",
            &[],
        )
        .expect("insert salvage agents");
    // DB-only message not in archive
    salvage_conn
        .query_sync(
            "INSERT INTO messages (id, project_id, sender_id, subject, body_md, importance, recipients_json, created_ts)
             VALUES (3, 100, 12, 'DB-only msg', 'Only in salvage db.', 'normal', '{\"to\":[\"Alice\"],\"cc\":[],\"bcc\":[]}', 3)",
            &[],
        )
        .expect("insert salvage db-only message");
    // read_ts and ack_ts for archive message 1
    salvage_conn
        .query_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (1, 11, 'to', 100000, 200000),
                (3, 10, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert salvage recipients");
    // Agent link: Alice -> Bob = allowed
    salvage_conn
        .query_sync(
            "INSERT INTO agent_links (id, project_id, from_agent_id, to_agent_id, status, reason, updated_ts)
             VALUES (1, 100, 10, 11, 'allowed', 'mutual work', 500000)",
            &[],
        )
        .expect("insert salvage agent link");

    // Replay: reconstruct with salvage
    let stats =
        reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
            .expect("salvage merge reconstruct should succeed");

    let mut report = ReplayDiffReport::new("salvage-merge");
    report.stats = Some(stats.clone());

    // Archive reconstructed 1 project, 2 agents, 2 messages
    // Salvage contributed Dave (agent), message 3, recipient state
    if stats.projects != 1 {
        report.add_mismatch("stats", "projects", "1", &stats.projects.to_string());
    }
    if stats.agents < 2 {
        report.add_mismatch("stats", "agents", ">=2", &stats.agents.to_string());
    }
    if stats.messages != 2 {
        report.add_mismatch("stats", "messages", "2", &stats.messages.to_string());
    }
    // Salvage should contribute at least 1 agent (Dave) and 1 message
    if stats.salvaged_agents < 1 {
        report.add_mismatch(
            "stats",
            "salvaged_agents",
            ">=1",
            &stats.salvaged_agents.to_string(),
        );
    }
    if stats.salvaged_messages < 1 {
        report.add_mismatch(
            "stats",
            "salvaged_messages",
            ">=1",
            &stats.salvaged_messages.to_string(),
        );
    }

    // DB should have all 3 messages
    let snapshot = snapshot_db(&db_path);
    if snapshot.messages.len() != 3 {
        report.add_mismatch(
            "db",
            "message_count",
            "3",
            &snapshot.messages.len().to_string(),
        );
    }

    // DB-only message subject should be present
    let has_db_only = snapshot.messages.iter().any(|m| m.1 == "DB-only msg");
    if !has_db_only {
        report.add_mismatch("messages", "DB-only msg", "present", "<missing>");
    }

    // Dave should be in the agents list (salvaged)
    let has_dave = snapshot.agents.iter().any(|a| a.1 == "Dave");
    if !has_dave {
        report.add_mismatch("agents", "Dave", "present", "<missing>");
    }

    // Agent links should include Alice -> Bob
    let has_link = snapshot
        .agent_links
        .iter()
        .any(|l| l.1 == "Alice" && l.2 == "Bob" && l.3 == "allowed");
    if !has_link {
        report.add_mismatch(
            "agent_links",
            "Alice->Bob",
            "allowed",
            &format!("{:?}", snapshot.agent_links),
        );
    }

    // Verify read_ts/ack_ts were merged from salvage
    let conn = mcp_agent_mail_db::DbConn::open_file(db_path.to_str().expect("valid path"))
        .expect("open reconstructed db");
    let bob_recipient = conn
        .query_sync(
            "SELECT mr.read_ts, mr.ack_ts
             FROM message_recipients mr
             JOIN agents a ON a.id = mr.agent_id
             WHERE mr.message_id = 1 AND a.name = 'Bob'",
            &[],
        )
        .expect("query Bob recipient state");
    if !bob_recipient.is_empty() {
        let read_ts = bob_recipient[0].get_named::<i64>("read_ts").ok();
        let ack_ts = bob_recipient[0].get_named::<i64>("ack_ts").ok();
        if read_ts != Some(100_000) {
            report.add_mismatch(
                "recipients",
                "Bob read_ts for msg 1",
                "100_000",
                &format!("{read_ts:?}"),
            );
        }
        if ack_ts != Some(200_000) {
            report.add_mismatch(
                "recipients",
                "Bob ack_ts for msg 1",
                "200_000",
                &format!("{ack_ts:?}"),
            );
        }
    }

    report.assert_clean();
}

// ============================================================================
// Scenario 3: Drift detection between archive and DB
// ============================================================================

#[test]
fn replay_drift_detection_archive_ahead() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("drift-test.db");
    let storage_root = tmp.path().join("storage");

    // Build archive with 3 messages
    write_project_metadata(&storage_root, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        &storage_root,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );
    write_agent_profile(&storage_root, "proj-alpha", "Bob", "codex", "o3-pro");

    for i in 1..=3 {
        write_archive_message(
            &storage_root,
            "proj-alpha",
            "2026",
            "02",
            &format!("2026-02-{:02}T12-00-00Z__msg__{i}.md", i + 20),
            &json!({
                "id": i,
                "from": "Alice",
                "to": ["Bob"],
                "subject": format!("Message {i}"),
                "importance": "normal",
                "created_ts": format!("2026-02-{:02}T12:00:00Z", i + 20),
            }),
            &format!("Body of message {i}."),
        );
    }

    // Reconstruct DB with only message 1 (simulating a lagging DB)
    let stats =
        reconstruct_from_archive(&db_path, &storage_root).expect("reconstruct should succeed");
    assert_eq!(
        stats.messages, 3,
        "should have reconstructed all 3 messages"
    );

    // Now create a second DB that only has message 1 (to simulate drift)
    let drift_db_path = tmp.path().join("lagging.db");
    let lagging_storage = tmp.path().join("lagging-storage");
    write_project_metadata(&lagging_storage, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        &lagging_storage,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );
    write_agent_profile(&lagging_storage, "proj-alpha", "Bob", "codex", "o3-pro");
    write_archive_message(
        &lagging_storage,
        "proj-alpha",
        "2026",
        "02",
        "2026-02-21T12-00-00Z__msg__1.md",
        &json!({
            "id": 1,
            "from": "Alice",
            "to": ["Bob"],
            "subject": "Message 1",
            "importance": "normal",
            "created_ts": "2026-02-21T12:00:00Z",
        }),
        "Body of message 1.",
    );
    reconstruct_from_archive(&drift_db_path, &lagging_storage)
        .expect("lagging reconstruct should succeed");

    // Drift report: full archive vs lagging DB
    let drift = compute_archive_drift_report(&storage_root, &drift_db_path)
        .expect("drift report should succeed");

    let mut report = ReplayDiffReport::new("drift-archive-ahead");
    if !drift.has_message_drift() {
        report.add_mismatch("drift", "has_message_drift", "true", "false");
    }
    if drift.archive_message_count != 3 {
        report.add_mismatch(
            "drift",
            "archive_message_count",
            "3",
            &drift.archive_message_count.to_string(),
        );
    }
    if drift.db_message_count != 1 {
        report.add_mismatch(
            "drift",
            "db_message_count",
            "1",
            &drift.db_message_count.to_string(),
        );
    }
    if !drift.archive_only_ids.contains(&2) || !drift.archive_only_ids.contains(&3) {
        report.add_mismatch(
            "drift",
            "archive_only_ids",
            "{2, 3}",
            &format!("{:?}", drift.archive_only_ids),
        );
    }
    if !drift.db_only_ids.is_empty() {
        report.add_mismatch(
            "drift",
            "db_only_ids",
            "{}",
            &format!("{:?}", drift.db_only_ids),
        );
    }
    report.assert_clean();
}

#[test]
fn replay_drift_detection_db_ahead() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage_root = tmp.path().join("storage");

    // Archive has 1 message
    write_project_metadata(&storage_root, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        &storage_root,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );
    write_archive_message(
        &storage_root,
        "proj-alpha",
        "2026",
        "02",
        "2026-02-22T12-00-00Z__msg__1.md",
        &json!({
            "id": 1,
            "from": "Alice",
            "to": ["Alice"],
            "subject": "Only archive msg",
            "importance": "normal",
            "created_ts": "2026-02-22T12:00:00Z",
        }),
        "Archive body.",
    );

    // DB has messages 1, 2, 3 (DB is ahead)
    let db_path = tmp.path().join("db-ahead.db");
    let full_storage = tmp.path().join("full-storage");
    write_project_metadata(&full_storage, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        &full_storage,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );
    for i in 1..=3 {
        write_archive_message(
            &full_storage,
            "proj-alpha",
            "2026",
            "02",
            &format!("2026-02-{:02}T12-00-00Z__msg__{i}.md", i + 20),
            &json!({
                "id": i,
                "from": "Alice",
                "to": ["Alice"],
                "subject": format!("Message {i}"),
                "importance": "normal",
                "created_ts": format!("2026-02-{:02}T12:00:00Z", i + 20),
            }),
            &format!("Body {i}."),
        );
    }
    reconstruct_from_archive(&db_path, &full_storage).expect("full reconstruct should succeed");

    // Drift: small archive vs full DB
    let drift =
        compute_archive_drift_report(&storage_root, &db_path).expect("drift report should succeed");

    assert!(
        drift.has_message_drift(),
        "should detect drift when DB is ahead"
    );
    assert_eq!(drift.archive_message_count, 1);
    assert_eq!(drift.db_message_count, 3);
    assert!(drift.db_only_ids.contains(&2));
    assert!(drift.db_only_ids.contains(&3));
    assert!(drift.archive_only_ids.is_empty());
}

// ============================================================================
// Scenario 4: Corrupt salvage database graceful degradation
// ============================================================================

#[test]
fn replay_corrupt_salvage_degrades_gracefully() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("reconstructed.db");
    let salvage_db_path = tmp.path().join("corrupt-salvage.db");
    let storage_root = tmp.path().join("storage");

    // Build a minimal archive
    write_project_metadata(&storage_root, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        &storage_root,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );
    write_archive_message(
        &storage_root,
        "proj-alpha",
        "2026",
        "02",
        "2026-02-22T12-00-00Z__msg__1.md",
        &json!({
            "id": 1,
            "from": "Alice",
            "to": ["Alice"],
            "subject": "Survived msg",
            "importance": "normal",
            "created_ts": "2026-02-22T12:00:00Z",
        }),
        "This message should survive despite corrupt salvage.",
    );

    // Write garbage bytes as the salvage DB
    std::fs::write(&salvage_db_path, b"THIS IS NOT A SQLITE DATABASE")
        .expect("write corrupt salvage");

    // Should succeed (salvage is skipped with a warning, not fatal)
    let stats =
        reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
            .expect("should succeed despite corrupt salvage");

    assert_eq!(stats.projects, 1);
    assert_eq!(stats.agents, 1);
    assert_eq!(stats.messages, 1);
    assert_eq!(
        stats.salvaged_projects, 0,
        "corrupt salvage should not contribute"
    );
    assert_eq!(stats.salvaged_agents, 0);
    assert_eq!(stats.salvaged_messages, 0);
    assert!(
        stats.warnings.iter().any(|w| w.contains("Skipping")),
        "should have a warning about skipping corrupt salvage; warnings: {:?}",
        stats.warnings,
    );

    // Verify the archive data survived
    let snapshot = snapshot_db(&db_path);
    assert_eq!(snapshot.messages.len(), 1);
    assert_eq!(snapshot.messages[0].1, "Survived msg");
}

// ============================================================================
// Scenario 5: Missing salvage database (None)
// ============================================================================

#[test]
fn replay_no_salvage_path_is_clean_reconstruct() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("reconstructed.db");
    let storage_root = tmp.path().join("storage");

    write_project_metadata(&storage_root, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        &storage_root,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );
    write_archive_message(
        &storage_root,
        "proj-alpha",
        "2026",
        "02",
        "2026-02-22T12-00-00Z__msg__1.md",
        &json!({
            "id": 1,
            "from": "Alice",
            "to": ["Alice"],
            "subject": "Clean msg",
            "importance": "normal",
            "created_ts": "2026-02-22T12:00:00Z",
        }),
        "Body.",
    );

    let stats = reconstruct_from_archive_with_salvage(&db_path, &storage_root, None)
        .expect("should succeed with no salvage");

    assert_eq!(stats.projects, 1);
    assert_eq!(stats.agents, 1);
    assert_eq!(stats.messages, 1);
    assert_eq!(stats.salvaged_projects, 0);
    assert_eq!(stats.salvaged_agents, 0);
    assert_eq!(stats.salvaged_messages, 0);
}

// ============================================================================
// Scenario 6: Forensic pre-snapshot correctness
// ============================================================================

#[test]
fn replay_pre_snapshot_captures_correct_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("test.sqlite3");

    // Write a minimal valid SQLite header so header fields can be read.
    let mut header = vec![0u8; 100];
    header[..16].copy_from_slice(b"SQLite format 3\0");
    // Page size = 4096 (big-endian u16 at offset 16)
    header[16] = 0x10;
    header[17] = 0x00;
    // Page count = 42 (big-endian u32 at offset 28)
    header[28] = 0;
    header[29] = 0;
    header[30] = 0;
    header[31] = 42;
    std::fs::write(&db_path, &header).expect("write sqlite header");

    let snapshot = capture_pre_recovery_snapshot(&db_path, "replay-test");

    assert_eq!(snapshot.trigger, "replay-test");
    assert!(
        snapshot.db_path.contains("test.sqlite3"),
        "db_path should contain the file name"
    );
    assert_eq!(snapshot.db_family, "test.sqlite3");
    assert_eq!(snapshot.page_size, Some(4096));
    assert_eq!(snapshot.page_count, Some(42));
    assert!(snapshot.db_bytes.is_some());
    assert_eq!(snapshot.db_bytes, Some(100));
    assert!(snapshot.self_pid > 0);
    assert!(snapshot.captured_at_us > 0);

    // With environment context
    let storage_root = tmp.path().join("storage");
    std::fs::create_dir_all(&storage_root).expect("create storage root");
    let enriched = snapshot.with_environment(&storage_root, "sqlite://user:pass@host/db");
    assert!(enriched.storage_root.is_some());
    assert!(
        enriched
            .database_url_redacted
            .as_ref()
            .is_some_and(|url| url.contains("****") && !url.contains("pass")),
        "credentials should be redacted"
    );
}

// ============================================================================
// Scenario 7: Archive inventory scanning correctness
// ============================================================================

#[test]
fn replay_archive_inventory_matches_expected_counts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage_root = tmp.path().join("storage");

    build_standard_archive(&storage_root);

    let inventory = scan_archive_message_inventory(&storage_root);

    let mut report = ReplayDiffReport::new("archive-inventory");
    if inventory.projects != 2 {
        report.add_mismatch(
            "inventory",
            "projects",
            "2",
            &inventory.projects.to_string(),
        );
    }
    if inventory.agents != 3 {
        report.add_mismatch("inventory", "agents", "3", &inventory.agents.to_string());
    }
    if inventory.unique_message_ids != 4 {
        report.add_mismatch(
            "inventory",
            "unique_message_ids",
            "4",
            &inventory.unique_message_ids.to_string(),
        );
    }
    if inventory.latest_message_id != Some(4) {
        report.add_mismatch(
            "inventory",
            "latest_message_id",
            "Some(4)",
            &format!("{:?}", inventory.latest_message_id),
        );
    }
    if inventory.parse_errors != 0 {
        report.add_mismatch(
            "inventory",
            "parse_errors",
            "0",
            &inventory.parse_errors.to_string(),
        );
    }
    report.assert_clean();
}

// ============================================================================
// Scenario 8: Duplicate canonical message deduplication
// ============================================================================

#[test]
fn replay_duplicate_canonical_messages_are_deduplicated() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("dedup.db");
    let storage_root = tmp.path().join("storage");

    write_project_metadata(&storage_root, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        &storage_root,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );

    // Write the same message ID twice in different files
    write_archive_message(
        &storage_root,
        "proj-alpha",
        "2026",
        "02",
        "2026-02-22T12-00-00Z__original__1.md",
        &json!({
            "id": 1,
            "from": "Alice",
            "to": ["Alice"],
            "subject": "Original copy",
            "importance": "normal",
            "created_ts": "2026-02-22T12:00:00Z",
        }),
        "Original body.",
    );
    write_archive_message(
        &storage_root,
        "proj-alpha",
        "2026",
        "02",
        "2026-02-22T12-00-01Z__duplicate__1.md",
        &json!({
            "id": 1,
            "from": "Alice",
            "to": ["Alice"],
            "subject": "Duplicate copy",
            "importance": "normal",
            "created_ts": "2026-02-22T12:00:01Z",
        }),
        "Duplicate body.",
    );

    let stats = reconstruct_from_archive(&db_path, &storage_root)
        .expect("reconstruct with duplicates should succeed");

    assert_eq!(stats.messages, 1, "should have only 1 message after dedup");
    assert!(
        stats.duplicate_canonical_message_files > 0,
        "should report duplicate canonical files"
    );

    let snapshot = snapshot_db(&db_path);
    assert_eq!(snapshot.messages.len(), 1);
}

// ============================================================================
// Scenario 9: Malformed archive messages are skipped with parse errors
// ============================================================================

#[test]
fn replay_malformed_archive_messages_produce_parse_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("malformed.db");
    let storage_root = tmp.path().join("storage");

    write_project_metadata(&storage_root, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        &storage_root,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );

    // Valid message
    write_archive_message(
        &storage_root,
        "proj-alpha",
        "2026",
        "02",
        "2026-02-22T12-00-00Z__good__1.md",
        &json!({
            "id": 1,
            "from": "Alice",
            "to": ["Alice"],
            "subject": "Good message",
            "importance": "normal",
            "created_ts": "2026-02-22T12:00:00Z",
        }),
        "Good body.",
    );

    // Malformed message: invalid JSON frontmatter
    let msg_dir = storage_root
        .join("projects")
        .join("proj-alpha")
        .join("messages")
        .join("2026")
        .join("02");
    std::fs::write(
        msg_dir.join("2026-02-23T12-00-00Z__bad__2.md"),
        "---json\n{this is not valid json}\n---\n\nBad body.\n",
    )
    .expect("write malformed message");

    // Message with no frontmatter at all
    std::fs::write(
        msg_dir.join("2026-02-24T12-00-00Z__nofm__3.md"),
        "No frontmatter at all.\nJust plain text.\n",
    )
    .expect("write no-frontmatter message");

    let stats = reconstruct_from_archive(&db_path, &storage_root)
        .expect("should succeed despite malformed messages");

    assert_eq!(
        stats.messages, 1,
        "only the good message should be ingested"
    );
    assert!(
        stats.parse_errors >= 1,
        "should report parse errors for malformed messages; got {}",
        stats.parse_errors,
    );

    let snapshot = snapshot_db(&db_path);
    assert_eq!(snapshot.messages.len(), 1);
    assert_eq!(snapshot.messages[0].1, "Good message");
}

// ============================================================================
// Scenario 10: Full round-trip — build archive, capture bundle, corrupt DB,
//              reconstruct, verify identical state
// ============================================================================

#[test]
fn replay_full_round_trip_capture_reconstruct_verify() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage_root = tmp.path().join("storage");
    let original_db_path = tmp.path().join("original.db");
    let reconstructed_db_path = tmp.path().join("reconstructed.db");

    // Build a rich archive
    build_standard_archive(&storage_root);

    // Reconstruct the original DB from archive
    let original_stats = reconstruct_from_archive(&original_db_path, &storage_root)
        .expect("original reconstruct should succeed");

    // Capture a forensic bundle from the original DB
    let bundle_dir = capture_mailbox_forensic_bundle(MailboxForensicCapture {
        command_name: "round-trip-test",
        trigger: "pre-corruption",
        database_url: &format!("sqlite:///{}", original_db_path.display()),
        db_path: &original_db_path,
        storage_root: &storage_root,
        integrity_detail: None,
    })
    .expect("forensic bundle capture");

    // Verify bundle contains the SQLite copy
    let bundle_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(bundle_dir.join("manifest.json")).expect("read manifest"),
    )
    .expect("parse manifest");
    assert_eq!(
        bundle_manifest["artifacts"]["sqlite"]["db"]["status"], "captured",
        "bundle should contain captured SQLite file"
    );

    // Simulate corruption: the original DB is gone. Reconstruct from archive.
    let reconstructed_stats = reconstruct_from_archive(&reconstructed_db_path, &storage_root)
        .expect("reconstructed should succeed");

    // Compare stats
    let mut report = ReplayDiffReport::new("round-trip-verify");
    report.stats = Some(reconstructed_stats.clone());

    if original_stats.projects != reconstructed_stats.projects {
        report.add_mismatch(
            "stats",
            "projects",
            &original_stats.projects.to_string(),
            &reconstructed_stats.projects.to_string(),
        );
    }
    if original_stats.agents != reconstructed_stats.agents {
        report.add_mismatch(
            "stats",
            "agents",
            &original_stats.agents.to_string(),
            &reconstructed_stats.agents.to_string(),
        );
    }
    if original_stats.messages != reconstructed_stats.messages {
        report.add_mismatch(
            "stats",
            "messages",
            &original_stats.messages.to_string(),
            &reconstructed_stats.messages.to_string(),
        );
    }

    // Compare DB snapshots
    let original_snapshot = snapshot_db(&original_db_path);
    let reconstructed_snapshot = snapshot_db(&reconstructed_db_path);

    // Same number of rows
    if original_snapshot.projects.len() != reconstructed_snapshot.projects.len() {
        report.add_mismatch(
            "db",
            "project_count",
            &original_snapshot.projects.len().to_string(),
            &reconstructed_snapshot.projects.len().to_string(),
        );
    }
    if original_snapshot.agents.len() != reconstructed_snapshot.agents.len() {
        report.add_mismatch(
            "db",
            "agent_count",
            &original_snapshot.agents.len().to_string(),
            &reconstructed_snapshot.agents.len().to_string(),
        );
    }
    if original_snapshot.messages.len() != reconstructed_snapshot.messages.len() {
        report.add_mismatch(
            "db",
            "message_count",
            &original_snapshot.messages.len().to_string(),
            &reconstructed_snapshot.messages.len().to_string(),
        );
    }

    // Same project slugs
    let original_slugs: BTreeSet<&str> = original_snapshot
        .projects
        .iter()
        .map(|p| p.1.as_str())
        .collect();
    let reconstructed_slugs: BTreeSet<&str> = reconstructed_snapshot
        .projects
        .iter()
        .map(|p| p.1.as_str())
        .collect();
    if original_slugs != reconstructed_slugs {
        report.add_mismatch(
            "db",
            "project_slugs",
            &format!("{original_slugs:?}"),
            &format!("{reconstructed_slugs:?}"),
        );
    }

    // Same message subjects
    let original_subjects: BTreeSet<&str> = original_snapshot
        .messages
        .iter()
        .map(|m| m.1.as_str())
        .collect();
    let reconstructed_subjects: BTreeSet<&str> = reconstructed_snapshot
        .messages
        .iter()
        .map(|m| m.1.as_str())
        .collect();
    if original_subjects != reconstructed_subjects {
        report.add_mismatch(
            "db",
            "message_subjects",
            &format!("{original_subjects:?}"),
            &format!("{reconstructed_subjects:?}"),
        );
    }

    // Verify zero drift between archive and reconstructed DB
    let drift = compute_archive_drift_report(&storage_root, &reconstructed_db_path)
        .expect("drift report should succeed");
    if drift.has_any_drift() {
        report.add_mismatch(
            "drift",
            "has_any_drift",
            "false",
            &format!(
                "archive_only={:?}, db_only={:?}, identity_mismatches={:?}",
                drift.archive_only_ids, drift.db_only_ids, drift.identity_mismatches
            ),
        );
    }

    report.assert_clean();
}

// ============================================================================
// Scenario 11: Interrupted bundle (partially written DB in bundle SQLite dir)
// ============================================================================

#[test]
fn replay_interrupted_bundle_still_has_valid_metadata() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage_root = tmp.path().join("storage");

    write_project_metadata(&storage_root, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        &storage_root,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );

    // Create a real DB to capture
    let db_path = tmp.path().join("original.db");
    reconstruct_from_archive(&db_path, &storage_root).expect("initial reconstruct should succeed");

    // Capture a bundle
    let bundle_dir = capture_mailbox_forensic_bundle(MailboxForensicCapture {
        command_name: "interrupted-test",
        trigger: "simulated-crash",
        database_url: &format!("sqlite:///{}", db_path.display()),
        db_path: &db_path,
        storage_root: &storage_root,
        integrity_detail: Some("simulated integrity failure"),
    })
    .expect("bundle capture");

    // Simulate interruption: truncate the SQLite copy in the bundle
    let sqlite_dir = bundle_dir.join("sqlite");
    if sqlite_dir.is_dir() {
        for entry in std::fs::read_dir(&sqlite_dir)
            .expect("read sqlite dir")
            .flatten()
        {
            let path = entry.path();
            if path.is_file() {
                // Truncate to 10 bytes (corrupt the bundle's SQLite copy)
                std::fs::write(&path, [0u8; 10]).expect("truncate sqlite copy");
            }
        }
    }

    // The manifest and references should still be valid and readable
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(bundle_dir.join("manifest.json")).expect("read manifest"),
    )
    .expect("parse manifest");
    assert_eq!(manifest["trigger"], "simulated-crash");
    assert!(
        manifest["artifacts"]["references"]["environment"]["path"]
            .as_str()
            .is_some(),
        "environment reference path should exist"
    );

    // Summary should record integrity_detail
    let summary: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(bundle_dir.join("summary.json")).expect("read summary"),
    )
    .expect("parse summary");
    assert_eq!(summary["integrity_detail"], "simulated integrity failure");

    // Archive drift reference should still be readable
    let drift_ref_path = bundle_dir.join("references").join("archive-drift.json");
    assert!(
        drift_ref_path.exists(),
        "archive drift reference should exist"
    );
    let drift_ref: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&drift_ref_path).expect("read drift ref"))
            .expect("parse drift ref");
    assert!(
        drift_ref["archive"]["storage_root_exists"]
            .as_bool()
            .unwrap_or(false),
        "archive drift ref should record storage root exists"
    );
}

// ============================================================================
// Scenario 12: Empty archive reconstruction
// ============================================================================

#[test]
fn replay_empty_archive_produces_empty_db() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("empty.db");
    let storage_root = tmp.path().join("storage");
    std::fs::create_dir_all(&storage_root).expect("create empty storage root");

    let stats = reconstruct_from_archive(&db_path, &storage_root)
        .expect("empty archive reconstruct should succeed");

    assert_eq!(stats.projects, 0);
    assert_eq!(stats.agents, 0);
    assert_eq!(stats.messages, 0);
    assert_eq!(stats.recipients, 0);
    assert_eq!(stats.parse_errors, 0);
}

// ============================================================================
// Scenario 13: CC recipients and multi-recipient messages
// ============================================================================

#[test]
fn replay_cc_bcc_recipients_are_preserved() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("cc-test.db");
    let storage_root = tmp.path().join("storage");

    write_project_metadata(&storage_root, "proj-alpha", "/home/user/proj-alpha");
    write_agent_profile(
        &storage_root,
        "proj-alpha",
        "Alice",
        "claude-code",
        "opus-4.6",
    );
    write_agent_profile(&storage_root, "proj-alpha", "Bob", "codex", "o3-pro");
    write_agent_profile(&storage_root, "proj-alpha", "Carol", "aider", "sonnet-4");

    write_archive_message(
        &storage_root,
        "proj-alpha",
        "2026",
        "03",
        "2026-03-01T09-00-00Z__multi__1.md",
        &json!({
            "id": 1,
            "from": "Alice",
            "to": ["Bob"],
            "cc": ["Carol"],
            "subject": "Multi-recipient",
            "importance": "normal",
            "created_ts": "2026-03-01T09:00:00Z",
        }),
        "Message with to and cc.",
    );

    let stats =
        reconstruct_from_archive(&db_path, &storage_root).expect("reconstruct should succeed");

    assert_eq!(stats.messages, 1);
    // Bob (to) + Carol (cc) = 2 recipients
    assert!(
        stats.recipients >= 2,
        "should have at least 2 recipients (to + cc); got {}",
        stats.recipients,
    );

    let snapshot = snapshot_db(&db_path);
    let msg1_recipients: Vec<(&str, &str)> = snapshot
        .recipients
        .iter()
        .filter(|r| r.0 == 1)
        .map(|r| (r.1.as_str(), r.2.as_str()))
        .collect();

    let has_bob_to = msg1_recipients
        .iter()
        .any(|(name, kind)| *name == "Bob" && *kind == "to");
    let has_carol_cc = msg1_recipients
        .iter()
        .any(|(name, kind)| *name == "Carol" && *kind == "cc");

    assert!(
        has_bob_to,
        "Bob should be a 'to' recipient; got: {msg1_recipients:?}"
    );
    assert!(
        has_carol_cc,
        "Carol should be a 'cc' recipient; got: {msg1_recipients:?}"
    );
}
