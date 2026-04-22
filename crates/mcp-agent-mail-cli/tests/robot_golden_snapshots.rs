#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use mcp_agent_mail_cli::robot::{
    AttachmentInfo, MessageContext, OutputFormat, ReservationEntry, RobotEnvelope, StatusData,
    ThreadMessage, ThreadSummary, format_output, format_output_md,
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR should be crates/mcp-agent-mail-cli")
        .to_path_buf()
}

fn update_goldens_requested() -> bool {
    std::env::var_os("UPDATE_GOLDENS").is_some() || std::env::var_os("UPDATE_GOLDEN").is_some()
}

fn assert_golden(rel_path: &str, actual: &str) {
    let path = repo_root().join("tests/golden/cli").join(rel_path);
    if update_goldens_requested() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create golden fixture directory");
        }
        std::fs::write(&path, actual).expect("update golden fixture");
        eprintln!("updated golden fixture: {}", path.display());
        return;
    }

    let mut expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read golden fixture {}: {err}", path.display()));
    if expected.ends_with('\n') && !actual.ends_with('\n') {
        expected.pop();
    }
    assert_eq!(
        expected,
        actual,
        "golden fixture mismatch for {}; rerun with UPDATE_GOLDENS=1",
        path.display()
    );
}

fn status_envelope() -> RobotEnvelope<StatusData> {
    let mut env = RobotEnvelope::new(
        "robot status",
        OutputFormat::Json,
        StatusData {
            health: "ok".to_string(),
            unread: 2,
            urgent: 1,
            ack_required: 1,
            ack_overdue: 0,
            active_reservations: 1,
            reservations_expiring_soon: 0,
            active_agents: 2,
            recent_messages: 3,
            my_reservations: vec![ReservationEntry {
                agent: Some("RedFox".to_string()),
                path: "crates/mcp-agent-mail-cli/src/**".to_string(),
                exclusive: true,
                remaining_seconds: 3600,
                remaining: Some("1h".to_string()),
                granted_at: Some("2026-01-02T03:00:00Z".to_string()),
            }],
            top_threads: vec![ThreadSummary {
                id: "br-robot-golden".to_string(),
                subject: "Freeze robot output".to_string(),
                participants: 2,
                messages: 3,
                last_activity: "2026-01-02T03:04:00Z".to_string(),
            }],
            anomalies: Vec::new(),
            recovery: None,
        },
    )
    .with_alert(
        "warn",
        "One reservation expires soon",
        Some("am robot reservations --expiring=30".to_string()),
    )
    .with_action("am robot inbox --urgent");
    env._meta.timestamp = "2026-01-02T03:04:05Z".to_string();
    env._meta.project = Some("/workspace/project-alpha".to_string());
    env._meta.agent = Some("RedFox".to_string());
    env
}

fn message_envelope() -> RobotEnvelope<MessageContext> {
    let mut env = RobotEnvelope::new(
        "robot message 101",
        OutputFormat::Markdown,
        MessageContext {
            id: 101,
            from: "BlueLake".to_string(),
            from_program: Some("claude-code".to_string()),
            from_model: Some("opus-4.6".to_string()),
            to: vec!["RedFox".to_string()],
            subject: "Robot golden fixture".to_string(),
            body: "Fixture body used to catch markdown drift.".to_string(),
            thread: "br-robot-golden".to_string(),
            position: 2,
            total_in_thread: 3,
            importance: "high".to_string(),
            ack_status: "required".to_string(),
            created: "2026-01-02T03:02:00Z".to_string(),
            age: "2m".to_string(),
            previous: Some("100".to_string()),
            next: Some("102".to_string()),
            attachments: vec![AttachmentInfo {
                name: "audit-notes.txt".to_string(),
                size: "128 B".to_string(),
                mime_type: "text/plain".to_string(),
            }],
        },
    );
    env._meta.timestamp = "2026-01-02T03:04:05Z".to_string();
    env._meta.project = Some("/workspace/project-alpha".to_string());
    env._meta.agent = Some("RedFox".to_string());
    env
}

fn thread_envelope() -> RobotEnvelope<Vec<ThreadMessage>> {
    let mut env = RobotEnvelope::new(
        "robot thread br-robot-golden",
        OutputFormat::Markdown,
        vec![
            ThreadMessage {
                position: 1,
                from: "BlueLake".to_string(),
                to: "RedFox".to_string(),
                age: "3m".to_string(),
                importance: "normal".to_string(),
                ack: "none".to_string(),
                subject: "Start robot golden thread".to_string(),
                body: Some("Opening note for the golden thread.".to_string()),
            },
            ThreadMessage {
                position: 2,
                from: "RedFox".to_string(),
                to: "BlueLake".to_string(),
                age: "1m".to_string(),
                importance: "high".to_string(),
                ack: "required".to_string(),
                subject: "Re: Start robot golden thread".to_string(),
                body: Some("Reply body that should remain stable.".to_string()),
            },
        ],
    );
    env._meta.timestamp = "2026-01-02T03:04:05Z".to_string();
    env._meta.project = Some("/workspace/project-alpha".to_string());
    env._meta.agent = Some("RedFox".to_string());
    env
}

#[test]
fn robot_status_json_matches_golden() {
    let actual = format_output(&status_envelope(), OutputFormat::Json).expect("format json");
    assert_golden("robot/status/json.json", &actual);
}

#[test]
fn robot_status_toon_matches_golden() {
    let actual = format_output(&status_envelope(), OutputFormat::Toon).expect("format toon");
    assert_golden("robot/status/toon.toon", &actual);
}

#[test]
fn robot_message_markdown_matches_golden() {
    let actual =
        format_output_md(&message_envelope(), OutputFormat::Markdown).expect("format markdown");
    assert_golden("robot/message/md.md", &actual);
}

#[test]
fn robot_thread_markdown_matches_golden() {
    let actual =
        format_output_md(&thread_envelope(), OutputFormat::Markdown).expect("format markdown");
    assert_golden("robot/thread/md.md", &actual);
}
