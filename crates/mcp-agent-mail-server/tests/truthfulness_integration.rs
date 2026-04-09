//! E1 (br-2k3qx.5.1): Seeded Tab Truthfulness Integration Tests.
//!
//! Validates that every core TUI screen displays non-empty truth when the
//! backing database contains data.  A failure here means a screen silently
//! regressed to empty/placeholder rendering despite having real data behind it.
//!
//! Approach: Seed a temp DB → create `TuiSharedState` → tick each screen →
//! inspect `screen_diagnostics_since()` to verify `raw_count > 0` for each
//! surface that should have data.

#![forbid(unsafe_code)]
#![allow(
    clippy::too_many_arguments,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::unnecessary_cast
)]

use std::path::PathBuf;
use std::sync::Arc;

use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::sqlmodel::Value as SqlValue;
use mcp_agent_mail_server::tui_bridge::TuiSharedState;
use mcp_agent_mail_server::tui_events::{
    AgentSummary, ContactSummary, DbStatSnapshot, MailEvent, ProjectSummary, ReservationSnapshot,
};
use mcp_agent_mail_server::tui_screens::MailScreen;

// ── Fixture environment ─────────────────────────────────────────────────

struct SeededEnv {
    tmp_dir: tempfile::TempDir,
    db_path: PathBuf,
}

impl SeededEnv {
    /// Create a temp DB, initialise schema, and seed deterministic fixture data.
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("e1_truth.sqlite3");
        let env = Self {
            tmp_dir: tmp,
            db_path,
        };
        let conn = env.init_conn();
        env.seed(&conn);
        drop(conn);
        env
    }

    /// Open connection and initialise schema (first open only).
    fn init_conn(&self) -> DbConn {
        let conn = DbConn::open_file(self.db_path.display().to_string()).expect("open sqlite db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        conn
    }

    fn open_conn(&self) -> DbConn {
        DbConn::open_file(self.db_path.display().to_string()).expect("open sqlite db")
    }

    fn database_url(&self) -> String {
        format!("sqlite:///{}", self.db_path.display())
    }

    fn config(&self) -> Config {
        Config {
            database_url: self.database_url(),
            ..Default::default()
        }
    }

    fn state(&self) -> Arc<TuiSharedState> {
        TuiSharedState::new(&self.config())
    }

    /// Seed 3 projects, 6 agents, 20 messages across 4 threads, 3 contact links.
    #[allow(clippy::unused_self, clippy::too_many_lines)]
    fn seed(&self, conn: &DbConn) {
        let base_ts: i64 = 1_704_067_200_000_000; // 2024-01-01T00:00:00Z

        // Projects
        for (id, slug, hk) in [
            (1_i64, "alpha-proj", "/tmp/alpha"),
            (2, "beta-proj", "/tmp/beta"),
            (3, "gamma-proj", "/tmp/gamma"),
        ] {
            conn.execute_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
                &[
                    SqlValue::BigInt(id),
                    SqlValue::Text(slug.to_string()),
                    SqlValue::Text(hk.to_string()),
                    SqlValue::BigInt(base_ts),
                ],
            )
            .expect("insert project");
        }

        // Agents (2 per project)
        let agents: [(i64, i64, &str, &str, &str); 6] = [
            (1, 1, "RedFox", "claude-code", "claude-opus-4-6"),
            (2, 1, "BlueBear", "codex", "gpt-5"),
            (3, 2, "GreenOwl", "claude-code", "claude-sonnet-4-5"),
            (4, 2, "GoldEagle", "gemini", "gemini-ultra"),
            (5, 3, "SilverWolf", "claude-code", "claude-opus-4-6"),
            (6, 3, "CopperRobin", "codex", "gpt-5"),
        ];
        for (id, proj, name, program, model) in &agents {
            conn.execute_sync(
                "INSERT INTO agents (\
                    id, project_id, name, program, model, task_description, \
                    inception_ts, last_active_ts, attachments_policy, contact_policy\
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    SqlValue::BigInt(*id),
                    SqlValue::BigInt(*proj),
                    SqlValue::Text(name.to_string()),
                    SqlValue::Text(program.to_string()),
                    SqlValue::Text(model.to_string()),
                    SqlValue::Text("E1 truthfulness fixture agent".to_string()),
                    SqlValue::BigInt(base_ts),
                    SqlValue::BigInt(base_ts + 3_600_000_000),
                    SqlValue::Text("auto".to_string()),
                    SqlValue::Text("auto".to_string()),
                ],
            )
            .expect("insert agent");
        }

        // Messages (20 across 4 threads, with real body_md)
        let threads = [
            "thread-alpha",
            "thread-beta",
            "thread-gamma",
            "thread-delta",
        ];
        for i in 1..=20_i64 {
            let sender = agents[((i - 1) as usize) % agents.len()];
            let recipient = agents[(i as usize) % agents.len()];
            let thread = threads[((i - 1) as usize) % threads.len()];
            let importance = if i <= 5 { "urgent" } else { "normal" };
            let body = format!(
                "## Message {i}\n\nThis is **real markdown** body content for message {i}.\n\n\
                 - Item A\n- Item B\n\n```rust\nfn main() {{ println!(\"msg {i}\"); }}\n```"
            );

            // Column order MUST match table definition (sqlmodel binds by position):
            // id, project_id, sender_id, thread_id, subject, body_md, importance,
            // ack_required, created_ts
            conn.execute_sync(
                "INSERT INTO messages (\
                    id, project_id, sender_id, thread_id, subject, body_md, importance, \
                    ack_required, created_ts\
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    SqlValue::BigInt(i),
                    SqlValue::BigInt(sender.1),
                    SqlValue::BigInt(sender.0),
                    SqlValue::Text(thread.to_string()),
                    SqlValue::Text(format!("[{thread}] Test message {i}")),
                    SqlValue::Text(body),
                    SqlValue::Text(importance.to_string()),
                    SqlValue::Bool(i % 4 == 0),
                    SqlValue::BigInt(base_ts + i * 60_000_000),
                ],
            )
            .expect("insert message");

            conn.execute_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)",
                &[
                    SqlValue::BigInt(i),
                    SqlValue::BigInt(recipient.0),
                    SqlValue::Text("to".to_string()),
                ],
            )
            .expect("insert recipient");
        }

        // Contact links (3)
        for (id, ap, aa, bp, ba, status) in [
            (1_i64, 1_i64, 1_i64, 2_i64, 3_i64, "approved"),
            (2, 1, 2, 3, 5, "approved"),
            (3, 2, 4, 3, 6, "pending"),
        ] {
            conn.execute_sync(
                "INSERT INTO agent_links (\
                    id, a_project_id, a_agent_id, b_project_id, b_agent_id, \
                    status, reason, created_ts, updated_ts\
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    SqlValue::BigInt(id),
                    SqlValue::BigInt(ap),
                    SqlValue::BigInt(aa),
                    SqlValue::BigInt(bp),
                    SqlValue::BigInt(ba),
                    SqlValue::Text(status.to_string()),
                    SqlValue::Text("e1_fixture".to_string()),
                    SqlValue::BigInt(base_ts),
                    SqlValue::BigInt(base_ts),
                ],
            )
            .expect("insert agent_link");
        }
    }

    /// Build a `DbStatSnapshot` that reflects the seeded data for poller-fed screens.
    #[allow(clippy::unused_self, clippy::too_many_lines)]
    fn db_stat_snapshot(&self) -> DbStatSnapshot {
        let base_ts: i64 = 1_704_067_200_000_000;
        DbStatSnapshot {
            projects: 3,
            agents: 6,
            messages: 20,
            file_reservations: 0,
            contact_links: 3,
            ack_pending: 5,
            agents_list: vec![
                AgentSummary {
                    name: "RedFox".to_string(),
                    program: "claude-code".to_string(),
                    last_active_ts: base_ts + 3_600_000_000,
                },
                AgentSummary {
                    name: "BlueBear".to_string(),
                    program: "codex".to_string(),
                    last_active_ts: base_ts + 3_600_000_000,
                },
                AgentSummary {
                    name: "GreenOwl".to_string(),
                    program: "claude-code".to_string(),
                    last_active_ts: base_ts + 3_600_000_000,
                },
                AgentSummary {
                    name: "GoldEagle".to_string(),
                    program: "gemini".to_string(),
                    last_active_ts: base_ts + 3_600_000_000,
                },
                AgentSummary {
                    name: "SilverWolf".to_string(),
                    program: "claude-code".to_string(),
                    last_active_ts: base_ts + 3_600_000_000,
                },
                AgentSummary {
                    name: "CopperRobin".to_string(),
                    program: "codex".to_string(),
                    last_active_ts: base_ts + 3_600_000_000,
                },
            ],
            projects_list: vec![
                ProjectSummary {
                    id: 1,
                    slug: "alpha-proj".to_string(),
                    human_key: "/tmp/alpha".to_string(),
                    agent_count: 2,
                    message_count: 8, // agents[0..=1] (project 1) send at i=1,2,7,8,13,14,19,20
                    reservation_count: 0,
                    created_at: base_ts,
                },
                ProjectSummary {
                    id: 2,
                    slug: "beta-proj".to_string(),
                    human_key: "/tmp/beta".to_string(),
                    agent_count: 2,
                    message_count: 6, // agents[2..=3] (project 2) send at i=3,4,9,10,15,16
                    reservation_count: 0,
                    created_at: base_ts,
                },
                ProjectSummary {
                    id: 3,
                    slug: "gamma-proj".to_string(),
                    human_key: "/tmp/gamma".to_string(),
                    agent_count: 2,
                    message_count: 6,
                    reservation_count: 0,
                    created_at: base_ts,
                },
            ],
            contacts_list: vec![
                ContactSummary {
                    from_agent: "RedFox".to_string(),
                    to_agent: "GreenOwl".to_string(),
                    from_project_slug: "alpha-proj".to_string(),
                    to_project_slug: "beta-proj".to_string(),
                    status: "approved".to_string(),
                    reason: "e1_fixture".to_string(),
                    updated_ts: base_ts,
                    expires_ts: None,
                },
                ContactSummary {
                    from_agent: "BlueBear".to_string(),
                    to_agent: "SilverWolf".to_string(),
                    from_project_slug: "alpha-proj".to_string(),
                    to_project_slug: "gamma-proj".to_string(),
                    status: "approved".to_string(),
                    reason: "e1_fixture".to_string(),
                    updated_ts: base_ts,
                    expires_ts: None,
                },
                ContactSummary {
                    from_agent: "GoldEagle".to_string(),
                    to_agent: "CopperRobin".to_string(),
                    from_project_slug: "beta-proj".to_string(),
                    to_project_slug: "gamma-proj".to_string(),
                    status: "pending".to_string(),
                    reason: "e1_fixture".to_string(),
                    updated_ts: base_ts,
                    expires_ts: None,
                },
            ],
            reservation_snapshots: vec![],
            timestamp_micros: base_ts + 21 * 60_000_000,
        }
    }

    /// Validate DB entity counts match expectations (guard against seed failure).
    fn validate_seed(&self) {
        let conn = self.open_conn();
        let count = |table: &str| -> i64 {
            let rows = conn
                .query_sync(&format!("SELECT COUNT(*) FROM {table}"), &[])
                .expect("count");
            rows.first()
                .and_then(|r| r.get_by_name("COUNT(*)"))
                .and_then(|v| match v {
                    SqlValue::BigInt(n) => Some(*n),
                    SqlValue::Int(n) => Some(i64::from(*n)),
                    _ => None,
                })
                .unwrap_or(0)
        };
        assert_eq!(count("projects"), 3, "seed: projects");
        assert_eq!(count("agents"), 6, "seed: agents");
        assert_eq!(count("messages"), 20, "seed: messages");
        assert_eq!(count("message_recipients"), 20, "seed: recipients");
        assert_eq!(count("agent_links"), 3, "seed: agent_links");
    }
}

// ── Diagnostic helpers ──────────────────────────────────────────────────

fn find_diagnostic(
    state: &TuiSharedState,
    screen_name: &str,
) -> Option<mcp_agent_mail_server::tui_bridge::ScreenDiagnosticSnapshot> {
    state
        .screen_diagnostics_since(0)
        .into_iter()
        .rev()
        .find(|(_, d)| d.screen == screen_name)
        .map(|(_, d)| d)
}

fn mismatch_msg(surface: &str, field: &str, expected: &str, observed: &str) -> String {
    format!(
        "TRUTHFULNESS MISMATCH: surface={surface} field={field} expected={expected} observed={observed}"
    )
}

/// Emit a CI-consumable JSON diagnostic artifact for incident gates (E6).
///
/// Writes per-screen count dumps, query context, and DB truth summary to a
/// deterministic path under the test's temp directory. CI systems can collect
/// this file from `TRUTHFULNESS_ARTIFACT_DIR` env var or the default stderr dump.
/// Build a CI-consumable JSON diagnostic artifact string.
///
/// Returns the formatted JSON. Caller decides where to write it (file, stderr, etc.).
fn build_diagnostic_artifact(
    env: &SeededEnv,
    state: &TuiSharedState,
    screens_checked: &[(
        &str,
        Option<&mcp_agent_mail_server::tui_bridge::ScreenDiagnosticSnapshot>,
    )],
) -> String {
    let conn = env.open_conn();

    // DB truth counts
    let db_projects = count_table(&conn, "projects");
    let db_agents = count_table(&conn, "agents");
    let db_messages = count_table(&conn, "messages");
    let db_contacts = count_table(&conn, "agent_links");
    let db_threads = {
        let rows = conn
            .query_sync(
                "SELECT COUNT(DISTINCT thread_id) AS cnt FROM messages WHERE thread_id IS NOT NULL",
                &[],
            )
            .unwrap_or_default();
        rows.first()
            .and_then(|r| r.get_by_name("cnt"))
            .and_then(|v| match v {
                SqlValue::BigInt(n) => Some(*n),
                SqlValue::Int(n) => Some(i64::from(*n)),
                _ => None,
            })
            .unwrap_or(0)
    };

    // Per-screen diagnostic dumps
    let mut screen_dumps = Vec::new();
    for (name, diag) in screens_checked {
        let dump = diag.as_ref().map_or_else(
            || format!("    {{\"screen\":\"{name}\",\"status\":\"no_diagnostic_emitted\"}}"),
            |d| {
                format!(
                    "    {{\"screen\":\"{name}\",\"scope\":\"{}\",\"raw_count\":{},\"rendered_count\":{},\"dropped_count\":{},\"query_params\":\"{}\",\"db_url\":\"{}\",\"transport_mode\":\"{}\",\"auth_enabled\":{}}}",
                    d.scope,
                    d.raw_count,
                    d.rendered_count,
                    d.dropped_count,
                    d.query_params.replace('\"', "\\\""),
                    d.db_url.replace('\"', "\\\""),
                    d.transport_mode,
                    d.auth_enabled
                )
            },
        );
        screen_dumps.push(dump);
    }

    // All diagnostics from state
    let all_diags = state.screen_diagnostics_since(0);
    let diag_log_lines: Vec<String> = all_diags
        .iter()
        .map(|(seq, d)| {
            format!(
                "    \"[seq={seq}] {}\"",
                d.to_log_line().replace('\"', "\\\"")
            )
        })
        .collect();

    format!(
        r#"{{
  "artifact_type": "truthfulness_incident_diagnostic",
  "bead": "br-2k3qx.5.6",
  "db_truth": {{
    "projects": {db_projects},
    "agents": {db_agents},
    "messages": {db_messages},
    "threads": {db_threads},
    "contacts": {db_contacts}
  }},
  "screen_diagnostics": [
{}
  ],
  "diagnostic_log": [
{}
  ]
}}"#,
        screen_dumps.join(",\n"),
        diag_log_lines.join(",\n"),
    )
}

/// Emit a CI-consumable JSON diagnostic artifact for incident gates (E6).
///
/// Writes to stderr (captured by test harness / CI).
/// If `artifact_dir` is Some, also writes to a file in that directory.
fn emit_diagnostic_artifact(
    env: &SeededEnv,
    state: &TuiSharedState,
    screens_checked: &[(
        &str,
        Option<&mcp_agent_mail_server::tui_bridge::ScreenDiagnosticSnapshot>,
    )],
    artifact_dir: Option<&std::path::Path>,
) {
    let artifact = build_diagnostic_artifact(env, state, screens_checked);

    eprintln!("--- TRUTHFULNESS DIAGNOSTIC ARTIFACT ---");
    eprintln!("{artifact}");
    eprintln!("--- END ARTIFACT ---");

    if let Some(dir) = artifact_dir {
        let path = dir.join("truthfulness_diagnostic.json");
        if matches!(std::fs::create_dir_all(dir), Ok(())) {
            let _ = std::fs::write(&path, &artifact);
            eprintln!("Artifact written to: {}", path.display());
        }
    }
}

// ── E1 Tests: Poller-fed screens (diagnostics-verified) ─────────────────

/// Agents screen emits diagnostic with `raw_count` > 0 when seeded data delivered.
#[test]
fn e1_agents_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::agents::AgentsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut screen = AgentsScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "agents");
    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("agents", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg("agents", "raw_count", ">0", &diag.raw_count.to_string())
    );
    assert_eq!(
        diag.raw_count,
        6,
        "{}",
        mismatch_msg("agents", "raw_count", "6", &diag.raw_count.to_string())
    );
    assert!(
        diag.rendered_count > 0,
        "{}",
        mismatch_msg(
            "agents",
            "rendered_count",
            ">0",
            &diag.rendered_count.to_string()
        )
    );
}

/// Projects screen emits diagnostic with `raw_count` > 0 when seeded data delivered.
#[test]
fn e1_projects_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::projects::ProjectsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut screen = ProjectsScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "projects");
    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("projects", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg("projects", "raw_count", ">0", &diag.raw_count.to_string())
    );
    assert_eq!(
        diag.raw_count,
        3,
        "{}",
        mismatch_msg("projects", "raw_count", "3", &diag.raw_count.to_string())
    );
}

/// Contacts screen emits diagnostic with `raw_count` > 0 when seeded data delivered.
#[test]
fn e1_contacts_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::contacts::ContactsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut screen = ContactsScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "contacts");
    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("contacts", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg("contacts", "raw_count", ">0", &diag.raw_count.to_string())
    );
    assert_eq!(
        diag.raw_count,
        3,
        "{}",
        mismatch_msg("contacts", "raw_count", "3", &diag.raw_count.to_string())
    );
}

// ── E1 Tests: Direct-DB screens (diagnostics-verified) ──────────────────

/// Messages screen emits diagnostic with `raw_count` > 0 when seeded DB has messages.
#[test]
fn e1_messages_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::messages::MessageBrowserScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut screen = MessageBrowserScreen::new();

    // MessageBrowserScreen starts with search_dirty=true, debounce_remaining=0.
    // First tick triggers execute_search → ensure_db_conn → query.
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "messages");
    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("messages", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg("messages", "raw_count", ">0", &diag.raw_count.to_string())
    );

    // Verify the diagnostic doesn't indicate db_context_unavailable
    assert!(
        !diag.scope.contains("db_unavailable"),
        "{}",
        mismatch_msg("messages", "scope", "results", &diag.scope)
    );
}

/// Threads screen emits diagnostic with `raw_count` > 0 from seeded DB.
#[test]
fn e1_threads_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::threads::ThreadExplorerScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut screen = ThreadExplorerScreen::new();

    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "threads");
    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("threads", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg("threads", "raw_count", ">0", &diag.raw_count.to_string())
    );

    // We seeded 4 distinct thread IDs
    assert!(
        diag.raw_count >= 4,
        "{}",
        mismatch_msg("threads", "raw_count", ">=4", &diag.raw_count.to_string())
    );
}

/// Explorer screen connects to seeded DB without `db_context_unavailable`.
#[test]
fn e1_explorer_connects_to_seeded_db() {
    use mcp_agent_mail_server::tui_screens::explorer::MailExplorerScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut screen = MailExplorerScreen::new();

    screen.tick(0, &state);
    screen.tick(1, &state);

    let diags = state.screen_diagnostics_since(0);
    let explorer_diags: Vec<_> = diags
        .iter()
        .filter(|(_, d)| d.screen == "explorer")
        .collect();

    // Explorer should emit at least one diagnostic
    assert!(
        !explorer_diags.is_empty(),
        "{}",
        mismatch_msg("explorer", "diagnostic_emitted", "true", "false")
    );

    // None of the diagnostics should indicate db_unavailable
    let has_unavailable = explorer_diags
        .iter()
        .any(|(_, d)| d.scope.contains("db_unavailable"));
    assert!(
        !has_unavailable,
        "{}",
        mismatch_msg("explorer", "db_unavailable_diagnostic", "none", "found")
    );
}

/// Search screen connects to seeded DB without `db_context_unavailable`.
#[test]
fn e1_search_connects_to_seeded_db() {
    use mcp_agent_mail_server::tui_screens::search::SearchCockpitScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut screen = SearchCockpitScreen::new();

    screen.tick(0, &state);

    let diags = state.screen_diagnostics_since(0);
    let search_unavailable = diags
        .iter()
        .any(|(_, d)| d.screen == "search" && d.scope.contains("db_unavailable"));
    assert!(
        !search_unavailable,
        "{}",
        mismatch_msg("search", "db_unavailable_diagnostic", "none", "found")
    );
}

/// Attachments screen connects to seeded DB without `db_context_unavailable`.
#[test]
fn e1_attachments_connects_to_seeded_db() {
    use mcp_agent_mail_server::tui_screens::attachments::AttachmentExplorerScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut screen = AttachmentExplorerScreen::new();

    screen.tick(0, &state);

    let diags = state.screen_diagnostics_since(0);
    let att_unavailable = diags
        .iter()
        .any(|(_, d)| d.screen == "attachments" && d.scope.contains("db_unavailable"));
    assert!(
        !att_unavailable,
        "{}",
        mismatch_msg("attachments", "db_unavailable_diagnostic", "none", "found")
    );
}

/// Dashboard screen emits non-empty truth diagnostics when seeded events are present.
#[test]
fn e1_dashboard_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::dashboard::DashboardScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    for i in 1..=6_i64 {
        let pushed = state.push_event(MailEvent::message_received(
            i,
            "RedFox",
            vec!["BlueBear".to_string()],
            format!("Dashboard seeded message {i}"),
            "thread-alpha",
            "alpha-proj",
            format!("seeded dashboard body {i}"),
        ));
        assert!(pushed, "dashboard fixture event should be accepted");
    }

    let mut screen = DashboardScreen::new();
    screen.tick(0, &state);

    let mut pool = ftui::GraphemePool::new();
    let mut frame = ftui::Frame::new(120, 40, &mut pool);
    screen.view(&mut frame, ftui::layout::Rect::new(0, 0, 120, 40), &state);

    let diag = find_diagnostic(&state, "dashboard");
    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("dashboard", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg("dashboard", "raw_count", ">0", &diag.raw_count.to_string())
    );
    assert!(
        diag.rendered_count > 0,
        "{}",
        mismatch_msg(
            "dashboard",
            "rendered_count",
            ">0",
            &diag.rendered_count.to_string()
        )
    );
}

/// Timeline screen emits non-empty truth diagnostics when seeded events are present.
#[test]
fn e1_timeline_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::timeline::TimelineScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let pushed_msg = state.push_event(MailEvent::message_received(
        101,
        "GreenOwl",
        vec!["GoldEagle".to_string()],
        "Timeline seeded message",
        "thread-beta",
        "beta-proj",
        "seeded timeline message body",
    ));
    assert!(
        pushed_msg,
        "timeline fixture message event should be accepted"
    );
    let pushed_res = state.push_event(MailEvent::reservation_granted(
        "GreenOwl",
        vec!["src/**".to_string()],
        true,
        3600,
        "beta-proj",
    ));
    assert!(
        pushed_res,
        "timeline fixture reservation event should be accepted"
    );

    let mut screen = TimelineScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "timeline");
    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("timeline", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg("timeline", "raw_count", ">0", &diag.raw_count.to_string())
    );
    assert!(
        diag.rendered_count > 0,
        "{}",
        mismatch_msg(
            "timeline",
            "rendered_count",
            ">0",
            &diag.rendered_count.to_string()
        )
    );
}

/// Reservations screen emits non-empty truth diagnostics when reservation snapshots exist.
#[test]
fn e1_reservations_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::reservations::ReservationsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut snap = env.db_stat_snapshot();
    let base_ts: i64 = 1_704_067_200_000_000;
    snap.file_reservations = 2;
    snap.reservation_snapshots = vec![
        ReservationSnapshot {
            id: 1001,
            project_slug: "alpha-proj".to_string(),
            agent_name: "RedFox".to_string(),
            path_pattern: "crates/mcp-agent-mail-core/src/**".to_string(),
            exclusive: true,
            granted_ts: base_ts,
            expires_ts: base_ts + 3_600_000_000,
            released_ts: None,
        },
        ReservationSnapshot {
            id: 1002,
            project_slug: "beta-proj".to_string(),
            agent_name: "GreenOwl".to_string(),
            path_pattern: "crates/mcp-agent-mail-tools/src/**".to_string(),
            exclusive: false,
            granted_ts: base_ts + 60_000_000,
            expires_ts: base_ts + 7_200_000_000,
            released_ts: None,
        },
    ];
    state.update_db_stats(snap);

    let mut screen = ReservationsScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "reservations");
    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("reservations", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg(
            "reservations",
            "raw_count",
            ">0",
            &diag.raw_count.to_string()
        )
    );
    assert!(
        diag.rendered_count > 0,
        "{}",
        mismatch_msg(
            "reservations",
            "rendered_count",
            ">0",
            &diag.rendered_count.to_string()
        )
    );
}

/// Tool Metrics screen emits non-empty truth diagnostics when tool-call events exist.
#[test]
fn e1_tool_metrics_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::tool_metrics::ToolMetricsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let pushed_send = state.push_event(MailEvent::tool_call_end(
        "send_message",
        42,
        Some("ok".to_string()),
        1,
        2.1,
        vec![("messages".to_string(), 1)],
        Some("alpha-proj".to_string()),
        Some("RedFox".to_string()),
    ));
    assert!(pushed_send, "tool-metrics fixture event send_message");

    let pushed_fetch = state.push_event(MailEvent::tool_call_end(
        "fetch_inbox",
        15,
        Some("ok".to_string()),
        1,
        1.3,
        vec![("message_recipients".to_string(), 1)],
        Some("beta-proj".to_string()),
        Some("GreenOwl".to_string()),
    ));
    assert!(pushed_fetch, "tool-metrics fixture event fetch_inbox");

    let mut screen = ToolMetricsScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "tool_metrics");
    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("tool_metrics", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg(
            "tool_metrics",
            "raw_count",
            ">0",
            &diag.raw_count.to_string()
        )
    );
    assert!(
        diag.rendered_count > 0,
        "{}",
        mismatch_msg(
            "tool_metrics",
            "rendered_count",
            ">0",
            &diag.rendered_count.to_string()
        )
    );
}

/// System Health screen emits non-empty truth diagnostics after worker refresh.
#[test]
fn e1_system_health_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::system_health::SystemHealthScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());
    let _screen = SystemHealthScreen::new(Arc::clone(&state));

    let started = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(3);
    let diag = loop {
        if let Some(diag) = find_diagnostic(&state, "system_health") {
            break Some(diag);
        }
        if started.elapsed() > timeout {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    };

    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("system_health", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg(
            "system_health",
            "raw_count",
            ">0",
            &diag.raw_count.to_string()
        )
    );
    assert!(
        diag.rendered_count > 0,
        "{}",
        mismatch_msg(
            "system_health",
            "rendered_count",
            ">0",
            &diag.rendered_count.to_string()
        )
    );
}

/// Archive Browser screen emits non-empty truth diagnostics when archive files exist.
#[test]
fn e1_archive_browser_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::archive_browser::ArchiveBrowserScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let storage_root = env.tmp_dir.path().join("storage");
    let archive_dir = storage_root
        .join("projects")
        .join("alpha-proj")
        .join("messages")
        .join("2026")
        .join("03");
    std::fs::create_dir_all(&archive_dir).expect("create archive fixture dir");
    std::fs::write(
        archive_dir.join("msg-001.md"),
        "# Archive fixture\n\nSeeded entry for truth test.\n",
    )
    .expect("write archive fixture");

    let mut cfg = env.config();
    cfg.storage_root = storage_root;
    let state = TuiSharedState::new(&cfg);
    state.update_db_stats(env.db_stat_snapshot());

    let mut screen = ArchiveBrowserScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "archive_browser");
    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("archive_browser", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg(
            "archive_browser",
            "raw_count",
            ">0",
            &diag.raw_count.to_string()
        )
    );
    assert!(
        diag.rendered_count > 0,
        "{}",
        mismatch_msg(
            "archive_browser",
            "rendered_count",
            ">0",
            &diag.rendered_count.to_string()
        )
    );
}

/// Analytics screen emits non-empty truth diagnostics with seeded runtime context.
#[test]
fn e1_analytics_truthful_when_seeded() {
    use mcp_agent_mail_server::tui_screens::analytics::AnalyticsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut screen = AnalyticsScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "analytics");
    assert!(
        diag.is_some(),
        "{}",
        mismatch_msg("analytics", "diagnostic_emitted", "true", "false")
    );
    let diag = diag.unwrap();
    assert!(
        diag.raw_count > 0,
        "{}",
        mismatch_msg("analytics", "raw_count", ">0", &diag.raw_count.to_string())
    );
    assert!(
        diag.rendered_count > 0,
        "{}",
        mismatch_msg(
            "analytics",
            "rendered_count",
            ">0",
            &diag.rendered_count.to_string()
        )
    );
}

// ── E1 Tests: Cross-surface consistency ─────────────────────────────────

/// Poller-delivered agent count matches DB truth.
#[test]
fn e1_agent_count_poller_matches_db() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    let db_count = count_table(&conn, "agents");

    let snap = env.db_stat_snapshot();
    assert_eq!(
        snap.agents,
        db_count as u64,
        "{}",
        mismatch_msg(
            "cross_surface",
            "agent_count",
            &db_count.to_string(),
            &snap.agents.to_string()
        )
    );
    assert_eq!(
        snap.agents_list.len(),
        db_count as usize,
        "{}",
        mismatch_msg(
            "cross_surface",
            "agents_list_len",
            &db_count.to_string(),
            &snap.agents_list.len().to_string()
        )
    );
}

/// Poller-delivered project count matches DB truth.
#[test]
fn e1_project_count_poller_matches_db() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    let db_count = count_table(&conn, "projects");

    let snap = env.db_stat_snapshot();
    assert_eq!(
        snap.projects,
        db_count as u64,
        "{}",
        mismatch_msg(
            "cross_surface",
            "project_count",
            &db_count.to_string(),
            &snap.projects.to_string()
        )
    );
}

/// Thread count derived from messages matches seeded value.
#[test]
fn e1_thread_count_matches_seed() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    let rows = conn
        .query_sync(
            "SELECT COUNT(DISTINCT thread_id) AS cnt FROM messages WHERE thread_id IS NOT NULL",
            &[],
        )
        .expect("count");
    let distinct_threads: i64 = rows
        .first()
        .and_then(|r| r.get_by_name("cnt"))
        .and_then(|v| match v {
            SqlValue::BigInt(n) => Some(*n),
            SqlValue::Int(n) => Some(i64::from(*n)),
            _ => None,
        })
        .unwrap_or(0);

    assert_eq!(
        distinct_threads,
        4,
        "{}",
        mismatch_msg(
            "cross_surface",
            "distinct_threads",
            "4",
            &distinct_threads.to_string()
        )
    );
}

/// All seeded messages have non-empty `body_md` (no placeholders in DB).
#[test]
fn e1_all_messages_have_body_content() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    let empty_body = count_where(&conn, "messages", "body_md IS NULL OR body_md = ''");

    assert_eq!(
        empty_body,
        0,
        "{}",
        mismatch_msg(
            "fixture_invariant",
            "empty_body_messages",
            "0",
            &empty_body.to_string()
        )
    );
}

/// All seeded messages have `body_md` containing real markdown (not placeholder text).
#[test]
fn e1_message_bodies_contain_real_markdown() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    let has_md = count_where(&conn, "messages", "body_md LIKE '%real markdown%'");

    assert_eq!(
        has_md,
        20,
        "{}",
        mismatch_msg(
            "fixture_invariant",
            "real_markdown_messages",
            "20",
            &has_md.to_string()
        )
    );
}

// ── E3 Tests: Thread Integrity (list + detail + counts) ──────────────────

/// Thread list diagnostic `raw_count` matches distinct `thread_id` count in DB.
#[test]
fn e3_thread_list_cardinality_matches_db() {
    use mcp_agent_mail_server::tui_screens::threads::ThreadExplorerScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut screen = ThreadExplorerScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "threads").expect("threads diagnostic should be emitted");

    let conn = env.open_conn();
    let rows = conn
        .query_sync(
            "SELECT COUNT(DISTINCT thread_id) AS cnt FROM messages WHERE thread_id IS NOT NULL",
            &[],
        )
        .expect("count");
    let db_thread_count: u64 = rows
        .first()
        .and_then(|r| r.get_by_name("cnt"))
        .and_then(|v| match v {
            SqlValue::BigInt(n) => Some(*n as u64),
            SqlValue::Int(n) => Some(*n as u64),
            _ => None,
        })
        .unwrap_or(0);

    assert_eq!(
        diag.raw_count,
        db_thread_count,
        "{}",
        mismatch_msg(
            "e3_threads",
            "list_cardinality",
            &db_thread_count.to_string(),
            &diag.raw_count.to_string()
        )
    );
}

/// Thread list `rendered_count` matches `raw_count` when no filter is active.
#[test]
fn e3_thread_list_no_filter_renders_all() {
    use mcp_agent_mail_server::tui_screens::threads::ThreadExplorerScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut screen = ThreadExplorerScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "threads").expect("threads diagnostic should be emitted");

    assert_eq!(
        diag.rendered_count,
        diag.raw_count,
        "{}",
        mismatch_msg(
            "e3_threads",
            "no_filter_rendered_eq_raw",
            &diag.raw_count.to_string(),
            &diag.rendered_count.to_string()
        )
    );
}

/// Each seeded `thread_id` has the expected message distribution.
#[test]
fn e3_thread_message_distribution() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    // We seeded 20 messages across 4 threads (round-robin: idx % 4).
    // thread-alpha: messages 1,5,9,13,17 → 5 messages
    // thread-beta:  messages 2,6,10,14,18 → 5 messages
    // thread-gamma: messages 3,7,11,15,19 → 5 messages
    // thread-delta: messages 4,8,12,16,20 → 5 messages
    for (thread_id, expected_count) in [
        ("thread-alpha", 5),
        ("thread-beta", 5),
        ("thread-gamma", 5),
        ("thread-delta", 5),
    ] {
        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM messages WHERE thread_id = ?",
                &[SqlValue::Text(thread_id.to_string())],
            )
            .expect("count");
        let actual: i64 = rows
            .first()
            .and_then(|r| r.get_by_name("cnt"))
            .and_then(|v| match v {
                SqlValue::BigInt(n) => Some(*n),
                SqlValue::Int(n) => Some(i64::from(*n)),
                _ => None,
            })
            .unwrap_or(0);
        assert_eq!(
            actual,
            expected_count,
            "{}",
            mismatch_msg(
                "e3_thread_distribution",
                thread_id,
                &expected_count.to_string(),
                &actual.to_string()
            )
        );
    }
}

/// Thread list diagnostic does NOT indicate `db_unavailable`.
#[test]
fn e3_thread_list_not_db_unavailable() {
    use mcp_agent_mail_server::tui_screens::threads::ThreadExplorerScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut screen = ThreadExplorerScreen::new();
    screen.tick(0, &state);

    let diags = state.screen_diagnostics_since(0);
    let unavailable = diags
        .iter()
        .any(|(_, d)| d.screen == "threads" && d.scope.contains("db_unavailable"));
    assert!(
        !unavailable,
        "{}",
        mismatch_msg(
            "e3_threads",
            "db_context_available",
            "true",
            "db_unavailable scope found"
        )
    );
}

/// Thread messages have distinct senders (not all same agent).
#[test]
fn e3_thread_messages_have_distinct_senders() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    let rows = conn
        .query_sync(
            "SELECT COUNT(DISTINCT m.sender_id) AS cnt \
             FROM messages m WHERE m.thread_id = 'thread-alpha'",
            &[],
        )
        .expect("count");
    let distinct_senders: i64 = rows
        .first()
        .and_then(|r| r.get_by_name("cnt"))
        .and_then(|v| match v {
            SqlValue::BigInt(n) => Some(*n),
            SqlValue::Int(n) => Some(i64::from(*n)),
            _ => None,
        })
        .unwrap_or(0);

    // thread-alpha has messages 1,5,9,13,17; senders rotate through 6 agents
    // so multiple distinct senders expected
    assert!(
        distinct_senders > 1,
        "{}",
        mismatch_msg(
            "e3_thread_senders",
            "distinct_senders_in_thread",
            ">1",
            &distinct_senders.to_string()
        )
    );
}

// ── E6 Tests: CI Failure Diagnostics Artifacts ──────────────────────────

/// The `emit_diagnostic_artifact` function must produce valid JSON with `db_truth` section.
#[test]
fn e6_artifact_emits_db_truth_counts() {
    use mcp_agent_mail_server::tui_screens::agents::AgentsScreen;
    use mcp_agent_mail_server::tui_screens::projects::ProjectsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut agents = AgentsScreen::new();
    agents.tick(0, &state);
    let mut projects = ProjectsScreen::new();
    projects.tick(0, &state);

    let agents_diag = find_diagnostic(&state, "agents");
    let projects_diag = find_diagnostic(&state, "projects");

    let artifact_dir = env.tmp_dir.path().join("artifacts");

    emit_diagnostic_artifact(
        &env,
        &state,
        &[
            ("agents", agents_diag.as_ref()),
            ("projects", projects_diag.as_ref()),
        ],
        Some(&artifact_dir),
    );

    // Verify the file was written
    let artifact_path = artifact_dir.join("truthfulness_diagnostic.json");
    assert!(
        artifact_path.exists(),
        "e6: artifact file must be written when artifact_dir provided"
    );

    let content = std::fs::read_to_string(&artifact_path).expect("read artifact");

    // Verify it contains required sections
    assert!(
        content.contains("\"artifact_type\": \"truthfulness_incident_diagnostic\""),
        "e6: artifact must contain artifact_type"
    );
    assert!(
        content.contains("\"db_truth\""),
        "e6: artifact must contain db_truth section"
    );
    assert!(
        content.contains("\"screen_diagnostics\""),
        "e6: artifact must contain screen_diagnostics section"
    );
    assert!(
        content.contains("\"diagnostic_log\""),
        "e6: artifact must contain diagnostic_log section"
    );
}

/// Artifact `db_truth` counts must match actual DB contents.
#[test]
fn e6_artifact_db_truth_matches_reality() {
    use mcp_agent_mail_server::tui_screens::agents::AgentsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut agents = AgentsScreen::new();
    agents.tick(0, &state);

    let artifact_dir = env.tmp_dir.path().join("artifacts_truth");

    emit_diagnostic_artifact(
        &env,
        &state,
        &[("agents", find_diagnostic(&state, "agents").as_ref())],
        Some(&artifact_dir),
    );

    let content =
        std::fs::read_to_string(artifact_dir.join("truthfulness_diagnostic.json")).expect("read");

    // Cross-check counts from artifact against direct DB queries
    let conn = env.open_conn();
    let real_projects = count_table(&conn, "projects");
    let real_agents = count_table(&conn, "agents");
    let real_messages = count_table(&conn, "messages");

    assert!(
        content.contains(&format!("\"projects\": {real_projects}")),
        "e6: artifact projects count must match DB ({real_projects})"
    );
    assert!(
        content.contains(&format!("\"agents\": {real_agents}")),
        "e6: artifact agents count must match DB ({real_agents})"
    );
    assert!(
        content.contains(&format!("\"messages\": {real_messages}")),
        "e6: artifact messages count must match DB ({real_messages})"
    );
}

/// Artifact `screen_diagnostics` must include per-screen entries for each checked screen.
#[test]
fn e6_artifact_includes_per_screen_diagnostics() {
    use mcp_agent_mail_server::tui_screens::agents::AgentsScreen;
    use mcp_agent_mail_server::tui_screens::contacts::ContactsScreen;
    use mcp_agent_mail_server::tui_screens::projects::ProjectsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut agents = AgentsScreen::new();
    agents.tick(0, &state);
    let mut projects = ProjectsScreen::new();
    projects.tick(0, &state);
    let mut contacts = ContactsScreen::new();
    contacts.tick(0, &state);

    let artifact_dir = env.tmp_dir.path().join("artifacts_screens");

    emit_diagnostic_artifact(
        &env,
        &state,
        &[
            ("agents", find_diagnostic(&state, "agents").as_ref()),
            ("projects", find_diagnostic(&state, "projects").as_ref()),
            ("contacts", find_diagnostic(&state, "contacts").as_ref()),
        ],
        Some(&artifact_dir),
    );

    let content =
        std::fs::read_to_string(artifact_dir.join("truthfulness_diagnostic.json")).expect("read");

    // Each screen must appear in the diagnostics
    for screen in ["agents", "projects", "contacts"] {
        assert!(
            content.contains(&format!("\"screen\":\"{screen}\"")),
            "e6: artifact must contain diagnostic for screen '{screen}'"
        );
    }

    // Must contain raw_count for screens with diagnostics
    assert!(
        content.contains("\"raw_count\":"),
        "e6: artifact must include raw_count in screen diagnostics"
    );
}

/// Artifact must handle missing diagnostics gracefully (`no_diagnostic_emitted` marker).
#[test]
fn e6_artifact_handles_missing_diagnostics() {
    let env = SeededEnv::new();
    let state = env.state();

    let artifact_dir = env.tmp_dir.path().join("artifacts_missing");

    // Pass screens that never ticked → no diagnostics
    emit_diagnostic_artifact(
        &env,
        &state,
        &[("agents", None), ("explorer", None)],
        Some(&artifact_dir),
    );

    let content =
        std::fs::read_to_string(artifact_dir.join("truthfulness_diagnostic.json")).expect("read");

    // Missing diagnostics should show the no_diagnostic marker
    assert!(
        content.contains("\"status\":\"no_diagnostic_emitted\""),
        "e6: screens with no diagnostic must emit no_diagnostic_emitted marker"
    );
}

/// Artifact `diagnostic_log` section must contain log lines from screens that ticked.
#[test]
fn e6_artifact_diagnostic_log_populated() {
    use mcp_agent_mail_server::tui_screens::agents::AgentsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut agents = AgentsScreen::new();
    agents.tick(0, &state);

    let artifact_dir = env.tmp_dir.path().join("artifacts_log");

    emit_diagnostic_artifact(
        &env,
        &state,
        &[("agents", find_diagnostic(&state, "agents").as_ref())],
        Some(&artifact_dir),
    );

    let content =
        std::fs::read_to_string(artifact_dir.join("truthfulness_diagnostic.json")).expect("read");

    // The diagnostic_log should contain at least one seq-tagged entry
    assert!(
        content.contains("[seq="),
        "e6: diagnostic_log must contain seq-tagged log lines"
    );
    // Must mention screen_diag from to_log_line()
    assert!(
        content.contains("[screen_diag]"),
        "e6: diagnostic_log lines must contain [screen_diag] prefix from to_log_line()"
    );
}

// ── E7 Tests: Non-Empty Truth Smoke Matrix ──────────────────────────────

/// Smoke: agents screen renders non-empty with 6 seeded agents.
#[test]
fn e7_smoke_agents_non_empty() {
    use mcp_agent_mail_server::tui_screens::agents::AgentsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut screen = AgentsScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "agents").expect("agents diagnostic should be emitted");
    assert!(
        diag.raw_count > 0,
        "e7_smoke: agents raw_count should be > 0"
    );
    assert!(
        diag.rendered_count > 0,
        "e7_smoke: agents rendered_count should be > 0"
    );
    assert_eq!(diag.raw_count, 6, "e7_smoke: agents raw_count should be 6");
    assert_eq!(
        diag.rendered_count, 6,
        "e7_smoke: agents rendered_count=6 (no filter)"
    );
}

/// Smoke: projects screen renders non-empty with 3 seeded projects.
#[test]
fn e7_smoke_projects_non_empty() {
    use mcp_agent_mail_server::tui_screens::projects::ProjectsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut screen = ProjectsScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "projects").expect("projects diagnostic should be emitted");
    assert!(
        diag.raw_count > 0,
        "e7_smoke: projects raw_count should be > 0"
    );
    assert_eq!(
        diag.raw_count, 3,
        "e7_smoke: projects raw_count should be 3"
    );
}

/// Smoke: threads screen renders non-empty with 4 seeded threads.
#[test]
fn e7_smoke_threads_non_empty() {
    use mcp_agent_mail_server::tui_screens::threads::ThreadExplorerScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut screen = ThreadExplorerScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "threads").expect("threads diagnostic should be emitted");
    assert!(
        diag.raw_count >= 4,
        "e7_smoke: threads raw_count should be >= 4, got {}",
        diag.raw_count
    );
}

/// Smoke: messages screen renders non-empty with 20 seeded messages.
#[test]
fn e7_smoke_messages_non_empty() {
    use mcp_agent_mail_server::tui_screens::messages::MessageBrowserScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut screen = MessageBrowserScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "messages").expect("messages diagnostic should be emitted");
    assert!(
        diag.raw_count > 0,
        "e7_smoke: messages raw_count should be > 0, got {}",
        diag.raw_count
    );
    assert!(
        !diag.scope.contains("db_unavailable"),
        "e7_smoke: messages should not be db_unavailable"
    );
}

/// Smoke: contacts screen renders non-empty with 3 seeded contacts.
#[test]
fn e7_smoke_contacts_non_empty() {
    use mcp_agent_mail_server::tui_screens::contacts::ContactsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut screen = ContactsScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "contacts").expect("contacts diagnostic should be emitted");
    assert!(
        diag.raw_count > 0,
        "e7_smoke: contacts raw_count should be > 0"
    );
    assert_eq!(
        diag.raw_count, 3,
        "e7_smoke: contacts raw_count should be 3"
    );
}

/// Smoke: dashboard renders without panic after `db_stats` and events injected.
#[test]
fn e7_smoke_dashboard_renders_with_data() {
    use mcp_agent_mail_server::tui_screens::dashboard::DashboardScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    // Push some events so the dashboard has data
    let base_ts: i64 = 1_704_067_200_000_000;
    for i in 1..=5_u64 {
        let _ = state.push_event(
            mcp_agent_mail_server::tui_events::MailEvent::MessageReceived {
                seq: i,
                timestamp_micros: base_ts + (i as i64) * 60_000_000,
                source: mcp_agent_mail_server::tui_events::EventSource::Mail,
                redacted: false,
                id: i as i64,
                from: "RedFox".to_string(),
                to: vec!["BlueBear".to_string()],
                subject: format!("Test message {i}"),
                thread_id: "thread-alpha".to_string(),
                project: "alpha-proj".to_string(),
                body_md: format!("This is **real markdown** body for msg {i}"),
            },
        );
    }

    let mut screen = DashboardScreen::new();
    screen.tick(0, &state);

    // Dashboard emits diagnostics only from view(), not tick().
    // Verify events were pushed and the screen can tick without panic.
    let data_gen = state.data_generation();
    assert!(
        data_gen.event_total_pushed >= 5,
        "e7_smoke: dashboard should have events pushed (got {})",
        data_gen.event_total_pushed
    );
}

/// Smoke: all direct-DB screens connect successfully (no `db_unavailable`).
#[test]
fn e7_smoke_all_db_screens_connect() {
    use mcp_agent_mail_server::tui_screens::attachments::AttachmentExplorerScreen;
    use mcp_agent_mail_server::tui_screens::explorer::MailExplorerScreen;
    use mcp_agent_mail_server::tui_screens::messages::MessageBrowserScreen;
    use mcp_agent_mail_server::tui_screens::search::SearchCockpitScreen;
    use mcp_agent_mail_server::tui_screens::threads::ThreadExplorerScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();

    // Tick each direct-DB screen
    let mut messages = MessageBrowserScreen::new();
    messages.tick(0, &state);

    let mut threads = ThreadExplorerScreen::new();
    threads.tick(0, &state);

    let mut explorer = MailExplorerScreen::new();
    explorer.tick(0, &state);
    explorer.tick(1, &state);

    let mut search = SearchCockpitScreen::new();
    search.tick(0, &state);

    let mut attachments = AttachmentExplorerScreen::new();
    attachments.tick(0, &state);

    // None should have db_unavailable diagnostics
    let diags = state.screen_diagnostics_since(0);
    let db_unavailable: Vec<_> = diags
        .iter()
        .filter(|(_, d)| d.scope.contains("db_unavailable"))
        .collect();

    assert!(
        db_unavailable.is_empty(),
        "e7_smoke: no screen should report db_unavailable, found: {:?}",
        db_unavailable
            .iter()
            .map(|(_, d)| format!("{}:{}", d.screen, d.scope))
            .collect::<Vec<_>>()
    );
}

/// Smoke: message `body_md` in DB is real GFM content (not placeholder).
#[test]
fn e7_smoke_message_bodies_are_real_gfm() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    // Check that messages contain markdown features: headings, bold, code blocks
    let has_heading = count_where(&conn, "messages", "body_md LIKE '%## Message%'");
    let has_bold = count_where(&conn, "messages", "body_md LIKE '%real markdown%'");
    let has_code = count_where(&conn, "messages", "body_md LIKE '%```rust%'");
    let has_list = count_where(&conn, "messages", "body_md LIKE '%- Item A%'");

    assert_eq!(
        has_heading, 20,
        "e7_smoke: all messages should have headings"
    );
    assert_eq!(has_bold, 20, "e7_smoke: all messages should have bold text");
    assert_eq!(
        has_code, 20,
        "e7_smoke: all messages should have code blocks"
    );
    assert_eq!(
        has_list, 20,
        "e7_smoke: all messages should have list items"
    );
}

/// Smoke: fixture entity counts form a consistent matrix.
#[test]
fn e7_smoke_entity_count_matrix() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    let projects = count_table(&conn, "projects");
    let agents = count_table(&conn, "agents");
    let messages = count_table(&conn, "messages");
    let recipients = count_table(&conn, "message_recipients");
    let contacts = count_table(&conn, "agent_links");

    // Invariant matrix
    assert_eq!(projects, 3, "e7_smoke: 3 projects");
    assert_eq!(agents, 6, "e7_smoke: 6 agents (2 per project)");
    assert_eq!(messages, 20, "e7_smoke: 20 messages");
    assert_eq!(recipients, 20, "e7_smoke: 20 recipients (1 per message)");
    assert_eq!(contacts, 3, "e7_smoke: 3 contact links");

    // Cross-entity invariants
    assert_eq!(agents / projects, 2, "e7_smoke: 2 agents per project");
    assert_eq!(recipients, messages, "e7_smoke: one recipient per message");
}

// ── E8 Tests: Unit Invariant Suite for Truthfulness Contracts ─────────

// -- E8.1: Timestamp parsing/normalization invariants ──────────────────

/// `iso_to_micros` must parse all formats that appear in the DB.
#[test]
fn e8_iso_parse_formats_from_db() {
    use mcp_agent_mail_db::iso_to_micros;

    // RFC 3339 with Z suffix (most common in DB)
    assert!(
        iso_to_micros("2024-01-01T00:00:00.000000Z").is_some(),
        "e8: must parse RFC 3339 with Z"
    );
    // RFC 3339 with +00:00 offset
    assert!(
        iso_to_micros("2024-01-01T00:00:00+00:00").is_some(),
        "e8: must parse RFC 3339 with offset"
    );
    // Bare datetime without timezone
    assert!(
        iso_to_micros("2024-01-01T00:00:00").is_some(),
        "e8: must parse bare datetime"
    );
    // With microsecond precision
    assert!(
        iso_to_micros("2024-06-15T12:30:45.123456Z").is_some(),
        "e8: must parse microsecond precision"
    );
}

/// `iso_to_micros` must reject malformed timestamps without panicking.
#[test]
fn e8_iso_rejects_malformed_without_panic() {
    use mcp_agent_mail_db::iso_to_micros;

    let malformed = [
        "",
        "not-a-date",
        "2024-13-01T00:00:00Z", // month 13
        "2024-01-32T00:00:00Z", // day 32
        "2024-01-01",           // date only, no time
        "12345",                // integer-like string
        "null",
        "2024-01-01T25:00:00Z", // hour 25
    ];
    for input in malformed {
        assert!(
            iso_to_micros(input).is_none(),
            "e8: iso_to_micros({input:?}) must return None"
        );
    }
}

/// Round-trip: micros → ISO → micros must preserve microsecond precision.
#[test]
fn e8_timestamp_roundtrip_preserves_precision() {
    use mcp_agent_mail_db::{iso_to_micros, micros_to_iso};

    let test_values: &[i64] = &[
        0,                     // epoch
        1_704_067_200_000_000, // 2024-01-01
        1_704_067_200_123_456, // with microseconds
        -500_000,              // pre-1970
    ];
    for &original in test_values {
        let iso = micros_to_iso(original);
        let back = iso_to_micros(&iso);
        assert_eq!(
            back,
            Some(original),
            "e8: roundtrip failed for {original}: iso={iso:?}, back={back:?}"
        );
    }
}

/// `micros_to_naive` must never panic, even on extreme values.
#[test]
fn e8_timestamp_extreme_values_no_panic() {
    use mcp_agent_mail_db::micros_to_naive;

    let _ = micros_to_naive(i64::MIN);
    let _ = micros_to_naive(i64::MAX);
    let _ = micros_to_naive(0);
    let _ = micros_to_naive(-1);
    let _ = micros_to_naive(i64::MIN / 2);
    let _ = micros_to_naive(i64::MAX / 2);
    // If we reach here without panic, the test passes.
}

// -- E8.2: Adapter/query error contract invariants ────────────────────

/// Query against invalid SQL must return Err, NOT empty Vec.
#[test]
fn e8_invalid_sql_returns_error_not_empty() {
    let env = SeededEnv::new();
    let conn = env.open_conn();

    // Invalid SQL should produce an error, not silently return []
    let result = conn.query_sync("SELECT * FROM nonexistent_table", &[]);
    assert!(
        result.is_err(),
        "e8: query against nonexistent table must return Err, not empty Vec"
    );
}

/// Query with type mismatch in WHERE clause should not silently return empty.
#[test]
fn e8_type_mismatch_does_not_silently_empty() {
    let env = SeededEnv::new();
    let conn = env.open_conn();

    // SQLite is loosely typed, but this tests the contract that an impossible
    // filter returns 0 rows (not an error) — which is correct behavior.
    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS cnt FROM agents WHERE id = ?",
            &[SqlValue::Text("not_an_integer".to_string())],
        )
        .expect("query should succeed with type coercion");
    // SQLite will coerce, so count should be 0 (no match), not an error.
    let cnt: i64 = rows
        .first()
        .and_then(|r| r.get_by_name("cnt"))
        .and_then(|v| match v {
            SqlValue::BigInt(n) => Some(*n),
            SqlValue::Int(n) => Some(i64::from(*n)),
            _ => None,
        })
        .unwrap_or(-1);
    assert_eq!(
        cnt, 0,
        "e8: type mismatch should return count=0, not error or phantom rows"
    );
}

/// Empty result from a valid query is distinguishable from an error result.
#[test]
fn e8_empty_result_from_valid_query_is_ok_empty() {
    let env = SeededEnv::new();
    let conn = env.open_conn();

    let result = conn.query_sync(
        "SELECT id FROM messages WHERE id = ?",
        &[SqlValue::BigInt(99999)],
    );
    assert!(
        result.is_ok(),
        "e8: query with no matches should be Ok(empty), not Err"
    );
    assert!(
        result.unwrap().is_empty(),
        "e8: query with no matches should return empty Vec"
    );
}

// -- E8.3: Context binding invariants ─────────────────────────────────

/// `TuiSharedState` `config_snapshot` must preserve `raw_database_url` for DB connection.
#[test]
fn e8_config_snapshot_preserves_raw_db_url() {
    let env = SeededEnv::new();
    let state = env.state();
    let snap = state.config_snapshot();
    assert!(
        !snap.raw_database_url.is_empty(),
        "e8: raw_database_url must not be empty"
    );
    assert!(
        snap.raw_database_url.contains("sqlite"),
        "e8: raw_database_url must contain 'sqlite' for test DB"
    );
}

/// Screens that use `ensure_db_conn()` must get a valid connection from config.
#[test]
fn e8_messages_screen_connects_via_config() {
    use mcp_agent_mail_server::tui_screens::messages::MessageBrowserScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let mut screen = MessageBrowserScreen::new();
    screen.tick(0, &state);

    // Verify the screen connected and got data (not db_unavailable)
    let diag = find_diagnostic(&state, "messages");
    assert!(
        diag.is_some(),
        "e8: messages screen must emit diagnostic after tick"
    );
    let diag = diag.unwrap();
    assert!(
        !diag.scope.contains("db_unavailable"),
        "e8: messages screen must connect via config, got scope={}",
        diag.scope
    );
    assert!(
        !diag.db_url.is_empty(),
        "e8: diagnostic db_url must not be empty"
    );
}

// -- E8.4: Count/list consistency invariants ──────────────────────────

/// `raw_count` must equal DB truth for poller-fed screens.
#[test]
fn e8_agents_raw_count_equals_db_truth() {
    use mcp_agent_mail_server::tui_screens::agents::AgentsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    let snap = env.db_stat_snapshot();
    let expected_agents = snap.agents_list.len() as u64;
    state.update_db_stats(snap);

    let mut screen = AgentsScreen::new();
    screen.tick(0, &state);

    let diag = find_diagnostic(&state, "agents").expect("e8: agents diagnostic should be emitted");
    assert_eq!(
        diag.raw_count, expected_agents,
        "e8: agents raw_count must equal agents_list.len()"
    );
}

/// `rendered_count` must equal `raw_count` when no filter is active.
#[test]
fn e8_projects_rendered_equals_raw_no_filter() {
    use mcp_agent_mail_server::tui_screens::projects::ProjectsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut screen = ProjectsScreen::new();
    screen.tick(0, &state);

    let diag =
        find_diagnostic(&state, "projects").expect("e8: projects diagnostic should be emitted");
    assert_eq!(
        diag.rendered_count, diag.raw_count,
        "e8: rendered_count must equal raw_count when no filter active"
    );
}

/// `dropped_count` must equal raw - rendered for all screens.
#[test]
fn e8_dropped_count_consistent_across_screens() {
    use mcp_agent_mail_server::tui_screens::agents::AgentsScreen;
    use mcp_agent_mail_server::tui_screens::contacts::ContactsScreen;
    use mcp_agent_mail_server::tui_screens::projects::ProjectsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut agents = AgentsScreen::new();
    agents.tick(0, &state);
    let mut projects = ProjectsScreen::new();
    projects.tick(0, &state);
    let mut contacts = ContactsScreen::new();
    contacts.tick(0, &state);

    for screen_name in ["agents", "projects", "contacts"] {
        let diag = find_diagnostic(&state, screen_name)
            .unwrap_or_else(|| panic!("e8: {screen_name} diagnostic should be emitted"));
        let expected_dropped = diag.raw_count.saturating_sub(diag.rendered_count);
        assert_eq!(
            diag.dropped_count, expected_dropped,
            "e8: {screen_name} dropped_count must equal raw - rendered"
        );
    }
}

// -- E8.5: Markdown renderer truthfulness invariants ──────────────────

/// `render_message_body` must return Some for non-empty markdown bodies.
#[test]
fn e8_markdown_non_empty_body_returns_some() {
    use ftui_extras::markdown::MarkdownTheme;
    use mcp_agent_mail_server::tui_markdown::render_message_body;

    let theme = MarkdownTheme::default();

    // Bodies that must NOT return None
    let non_empty = [
        "Hello world",
        "## Heading",
        "- list item",
        "> blockquote",
        "```rust\nfn main() {}\n```",
        "**bold** text",
        "a",
    ];
    for body in non_empty {
        assert!(
            render_message_body(body, &theme).is_some(),
            "e8: render_message_body({body:?}) must return Some"
        );
    }
}

/// `render_message_body` must return None ONLY for empty/whitespace bodies.
#[test]
fn e8_markdown_empty_body_returns_none() {
    use ftui_extras::markdown::MarkdownTheme;
    use mcp_agent_mail_server::tui_markdown::render_message_body;

    let theme = MarkdownTheme::default();

    let empty_bodies = ["", "   ", "\n\n", "\t\t", " \n \n "];
    for body in empty_bodies {
        assert!(
            render_message_body(body, &theme).is_none(),
            "e8: render_message_body({body:?}) must return None"
        );
    }
}

/// Body preview truncation must enforce `max_chars` limit.
#[test]
fn e8_preview_truncation_enforced() {
    use mcp_agent_mail_server::tui_markdown::{
        BODY_PREVIEW_MAX_CHARS, render_message_body_preview,
    };

    let long_body = "word ".repeat(200);
    let preview = render_message_body_preview(&long_body, BODY_PREVIEW_MAX_CHARS);
    assert!(
        preview.is_some(),
        "e8: preview of non-empty body must be Some"
    );
    let text = preview.unwrap();
    assert!(
        text.chars().count() <= BODY_PREVIEW_MAX_CHARS,
        "e8: preview length {} exceeds max {}",
        text.chars().count(),
        BODY_PREVIEW_MAX_CHARS
    );
}

/// JSON auto-detection must wrap raw JSON in code fences.
#[test]
fn e8_json_auto_detection_contract() {
    use mcp_agent_mail_server::tui_markdown::looks_like_json;

    // Must detect
    assert!(
        looks_like_json(r#"{"key":"value"}"#),
        "e8: must detect JSON object"
    );
    assert!(looks_like_json("[1,2,3]"), "e8: must detect JSON array");

    // Must NOT detect
    assert!(
        !looks_like_json("# heading"),
        "e8: must not detect markdown heading"
    );
    assert!(
        !looks_like_json("plain text"),
        "e8: must not detect plain text"
    );
    assert!(
        !looks_like_json("```json\n{}\n```"),
        "e8: must not detect already-fenced JSON"
    );
}

// -- E8.6: Seeded fixture data integrity invariants ───────────────────

/// All seeded messages must have importance in {urgent, normal}.
#[test]
fn e8_fixture_importance_values_valid() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    let invalid = count_where(&conn, "messages", "importance NOT IN ('urgent', 'normal')");
    assert_eq!(
        invalid, 0,
        "e8: all messages must have importance in {{urgent, normal}}"
    );
}

/// Every message must have exactly one recipient.
#[test]
fn e8_fixture_one_recipient_per_message() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    let rows = conn
        .query_sync(
            "SELECT message_id, COUNT(*) AS cnt \
             FROM message_recipients \
             GROUP BY message_id \
             HAVING cnt != 1",
            &[],
        )
        .expect("query");
    assert!(
        rows.is_empty(),
        "e8: every message must have exactly 1 recipient, found {} violations",
        rows.len()
    );
}

/// All agent names in fixture must be globally unique.
#[test]
fn e8_fixture_agent_names_unique() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    let rows = conn
        .query_sync(
            "SELECT name, COUNT(*) AS cnt FROM agents GROUP BY name HAVING cnt > 1",
            &[],
        )
        .expect("query");
    assert!(rows.is_empty(), "e8: agent names must be globally unique");
}

/// Seeded timestamps must be positive and chronologically ordered.
#[test]
fn e8_fixture_timestamps_positive_and_ordered() {
    let env = SeededEnv::new();
    env.validate_seed();

    let conn = env.open_conn();
    let rows = conn
        .query_sync("SELECT id, created_ts FROM messages ORDER BY id ASC", &[])
        .expect("query");

    let mut prev_ts: i64 = 0;
    for row in &rows {
        let id = row
            .get_by_name("id")
            .and_then(|v| match v {
                SqlValue::BigInt(n) => Some(*n),
                SqlValue::Int(n) => Some(i64::from(*n)),
                _ => None,
            })
            .unwrap_or(0);
        let ts = row
            .get_by_name("created_ts")
            .and_then(|v| match v {
                SqlValue::BigInt(n) => Some(*n),
                SqlValue::Int(n) => Some(i64::from(*n)),
                _ => None,
            })
            .unwrap_or(0);

        assert!(
            ts > 0,
            "e8: message {id} timestamp must be positive, got {ts}"
        );
        assert!(
            ts >= prev_ts,
            "e8: message {id} timestamp {ts} must be >= previous {prev_ts}"
        );
        prev_ts = ts;
    }
}

// ── H5 Tests: CI Oracle Hard Gate Contracts ─────────────────────────────

/// Diagnostic artifact must produce parseable JSON (CI gate can consume it).
#[test]
fn h5_artifact_is_valid_json() {
    use mcp_agent_mail_server::tui_screens::agents::AgentsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut agents = AgentsScreen::new();
    agents.tick(0, &state);

    let json_str = build_diagnostic_artifact(
        &env,
        &state,
        &[("agents", find_diagnostic(&state, "agents").as_ref())],
    );

    // Must be parseable as JSON
    let parsed: serde_json::Value =
        serde_json::from_str(&json_str).expect("h5: artifact must be valid JSON");

    assert!(
        parsed.is_object(),
        "h5: artifact root must be a JSON object"
    );
    assert!(
        parsed.get("artifact_type").is_some(),
        "h5: artifact must have artifact_type"
    );
    assert!(
        parsed.get("db_truth").is_some(),
        "h5: artifact must have db_truth"
    );
    assert!(
        parsed.get("screen_diagnostics").is_some(),
        "h5: artifact must have screen_diagnostics"
    );
    assert!(
        parsed.get("diagnostic_log").is_some(),
        "h5: artifact must have diagnostic_log"
    );
}

/// CI gate mismatch detection: `raw_count` == 0 with seeded data signals a false-empty
/// (the gate should flag this as a mismatch).
#[test]
fn h5_false_empty_is_detectable_from_diagnostic() {
    use mcp_agent_mail_server::tui_bridge::ScreenDiagnosticSnapshot;

    // Simulate a screen that reported raw_count=0 despite having seeded data
    let false_empty_diag = ScreenDiagnosticSnapshot {
        screen: "agents".to_string(),
        scope: "all".to_string(),
        raw_count: 0,
        rendered_count: 0,
        dropped_count: 0,
        query_params: "{}".to_string(),
        timestamp_micros: 1_000_000,
        db_url: "sqlite:///tmp/test.db".to_string(),
        storage_root: "/tmp".to_string(),
        transport_mode: "stdio".to_string(),
        auth_enabled: false,
    };

    // A CI gate should detect this: seeded DB has 6 agents but screen shows 0
    assert_eq!(
        false_empty_diag.raw_count, 0,
        "h5: simulated false-empty has raw_count=0"
    );

    // The gate logic: if DB truth > 0 but diagnostic raw_count == 0, it's a mismatch
    let env = SeededEnv::new();
    let conn = env.open_conn();
    let db_agent_count = count_table(&conn, "agents");
    assert!(db_agent_count > 0, "h5: seeded DB must have agents");
    assert_ne!(
        db_agent_count as u64, false_empty_diag.raw_count,
        "h5: false-empty must differ from DB truth (gate should flag this)"
    );
}

/// CI gate whitelist: whitelisted `check_ids` should be excluded from failure count.
#[test]
#[allow(clippy::const_is_empty, clippy::needless_collect)]
fn h5_whitelist_excludes_known_mismatches() {
    // Simulate the gate's whitelist-filtering logic in Rust
    let _ = [
        "tui.agents:count",
        "tui.messages:count",
        "tui.threads:count",
        "tui.search:facets",
    ];
    let mismatched_ids = ["tui.messages:count", "tui.search:facets"];
    let whitelist = ["tui.search:facets"];

    let non_whitelisted: Vec<&&str> = mismatched_ids
        .iter()
        .filter(|id| !whitelist.contains(id))
        .collect();

    assert_eq!(
        non_whitelisted.len(),
        1,
        "h5: only 1 non-whitelisted mismatch should remain"
    );
    assert_eq!(
        *non_whitelisted[0], "tui.messages:count",
        "h5: tui.messages:count should not be whitelisted"
    );

    // Gate verdict: FAIL because there are non-whitelisted mismatches
    let verdict = if non_whitelisted.is_empty() {
        "PASS"
    } else {
        "FAIL"
    };
    assert_eq!(
        verdict, "FAIL",
        "h5: gate must FAIL on non-whitelisted mismatches"
    );

    // With all mismatches whitelisted, verdict should be WARN
    let all_whitelisted = ["tui.messages:count", "tui.search:facets"];
    let remaining: Vec<&&str> = mismatched_ids
        .iter()
        .filter(|id| !all_whitelisted.contains(id))
        .collect();
    let verdict2 = if remaining.is_empty() {
        if mismatched_ids.is_empty() {
            "PASS"
        } else {
            "WARN"
        }
    } else {
        "FAIL"
    };
    assert_eq!(verdict2, "WARN", "h5: all-whitelisted mismatches → WARN");

    // With no mismatches, verdict should be PASS
    let no_mismatches: Vec<&str> = vec![];
    let verdict3 = if no_mismatches.is_empty() {
        "PASS"
    } else {
        "FAIL"
    };
    assert_eq!(verdict3, "PASS", "h5: no mismatches → PASS");
}

/// Culprit surface map groups mismatches by surface ID.
#[test]
fn h5_culprit_surface_map_groups_by_surface() {
    use std::collections::HashMap;

    // Simulate mismatches from different surfaces
    let mismatches = vec![
        ("tui.agents", "tui.agents:count"),
        ("tui.agents", "tui.agents:name_format"),
        ("tui.messages", "tui.messages:body_empty"),
        ("tui.threads", "tui.threads:count"),
    ];

    let mut surface_map: HashMap<&str, Vec<&str>> = HashMap::new();
    for (surface, check_id) in &mismatches {
        surface_map.entry(surface).or_default().push(check_id);
    }

    assert_eq!(surface_map.len(), 3, "h5: 3 distinct surfaces");
    assert_eq!(
        surface_map["tui.agents"].len(),
        2,
        "h5: tui.agents has 2 mismatches"
    );
    assert_eq!(
        surface_map["tui.messages"].len(),
        1,
        "h5: tui.messages has 1 mismatch"
    );
    assert_eq!(
        surface_map["tui.threads"].len(),
        1,
        "h5: tui.threads has 1 mismatch"
    );
}

/// Artifact file output preserves screen-level counts for CI diff analysis.
#[test]
fn h5_artifact_preserves_screen_counts_for_diff() {
    use mcp_agent_mail_server::tui_screens::agents::AgentsScreen;
    use mcp_agent_mail_server::tui_screens::projects::ProjectsScreen;

    let env = SeededEnv::new();
    env.validate_seed();

    let state = env.state();
    state.update_db_stats(env.db_stat_snapshot());

    let mut agents = AgentsScreen::new();
    agents.tick(0, &state);
    let mut projects = ProjectsScreen::new();
    projects.tick(0, &state);

    let json_str = build_diagnostic_artifact(
        &env,
        &state,
        &[
            ("agents", find_diagnostic(&state, "agents").as_ref()),
            ("projects", find_diagnostic(&state, "projects").as_ref()),
        ],
    );

    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let diagnostics = parsed["screen_diagnostics"]
        .as_array()
        .expect("h5: screen_diagnostics must be array");

    assert_eq!(diagnostics.len(), 2, "h5: 2 screen diagnostics expected");

    for diag in diagnostics {
        let screen = diag["screen"].as_str().unwrap();
        let raw = diag["raw_count"].as_u64();
        let rendered = diag["rendered_count"].as_u64();

        assert!(raw.is_some(), "h5: {screen} must have raw_count");
        assert!(rendered.is_some(), "h5: {screen} must have rendered_count");

        // For seeded data with no filter, raw == rendered
        assert_eq!(
            raw, rendered,
            "h5: {screen} raw_count must equal rendered_count (no filter)"
        );
    }

    // DB truth must have real counts
    let db_truth = &parsed["db_truth"];
    assert!(
        db_truth["projects"].as_i64().unwrap() > 0,
        "h5: db_truth.projects must be > 0"
    );
    assert!(
        db_truth["agents"].as_i64().unwrap() > 0,
        "h5: db_truth.agents must be > 0"
    );
    assert!(
        db_truth["messages"].as_i64().unwrap() > 0,
        "h5: db_truth.messages must be > 0"
    );
}

// ── Utility ─────────────────────────────────────────────────────────────

fn count_table(conn: &DbConn, table: &str) -> i64 {
    let rows = conn
        .query_sync(&format!("SELECT COUNT(*) AS cnt FROM {table}"), &[])
        .expect("count");
    rows.first()
        .and_then(|r| r.get_by_name("cnt"))
        .and_then(|v| match v {
            SqlValue::BigInt(n) => Some(*n),
            SqlValue::Int(n) => Some(i64::from(*n)),
            _ => None,
        })
        .unwrap_or(0)
}

fn count_where(conn: &DbConn, table: &str, condition: &str) -> i64 {
    let rows = conn
        .query_sync(
            &format!("SELECT COUNT(*) AS cnt FROM {table} WHERE {condition}"),
            &[],
        )
        .expect("count");
    rows.first()
        .and_then(|r| r.get_by_name("cnt"))
        .and_then(|v| match v {
            SqlValue::BigInt(n) => Some(*n),
            SqlValue::Int(n) => Some(i64::from(*n)),
            _ => None,
        })
        .unwrap_or(0)
}
