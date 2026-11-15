//! Live mailbox and delivery operations shared by the operator compose forms.
//!
//! Drafts identify both their mailbox and their project. Recipient suggestions
//! are advisory: delivery rechecks the live roster before creating or touching
//! the operator identity, then uses the normal messaging tool and archive path.

use std::collections::HashSet;
use std::path::PathBuf;

use fastmcp_core::block_on;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::sqlmodel_core::{Error as SqlError, Row, Value};
use mcp_agent_mail_db::timestamps::micros_to_iso;

use crate::tui_compose::{ComposeEnvelope, ComposeProject, OVERSEER_AGENT_NAME};
use crate::tui_screens::{OperatorTaskResult, operator_tool_context};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeAgentStatus {
    Active,
    Retired,
    Deregistered,
}

#[derive(Debug)]
pub struct ComposeAgent {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) status: ComposeAgentStatus,
}

impl ComposeAgent {
    pub(crate) fn require_active(&self) -> Result<(), String> {
        match self.status {
            ComposeAgentStatus::Active => Ok(()),
            ComposeAgentStatus::Retired => Err(format!(
                "Agent '{}' is retired. Unretire this agent before sending new messages.",
                self.name
            )),
            ComposeAgentStatus::Deregistered => Err(format!(
                "Agent '{}' is deregistered and cannot send or receive new messages. Choose an active agent.",
                self.name
            )),
        }
    }
}

/// Resolve names like the backend: the first registered case-insensitive
/// identity wins, even when a newer spelling is still active. Observe its
/// lifecycle ledger in the same statement as the profile. Read-only legacy
/// snapshots may predate the ledger and retirement column, so retain their
/// task-description tombstones without attempting a migration here.
pub fn query_compose_agent_rows(
    mut query: impl FnMut(&str, &[Value]) -> Result<Vec<Row>, SqlError>,
    source_filter: &str,
    params: &[Value],
) -> Result<Vec<ComposeAgent>, String> {
    let query_error = |error| format!("Failed to load agents for compose: {error}");
    let columns: HashSet<String> = query("PRAGMA table_info(agents)", &[])
        .map_err(query_error)?
        .iter()
        .map(|row| row.get_named::<String>("name"))
        .collect::<Result<_, _>>()
        .map_err(query_error)?;
    let has_ledger = !query(
        "SELECT 1 AS present FROM sqlite_master \
         WHERE type = 'table' AND name = 'agent_deregistrations' LIMIT 1",
        &[],
    )
    .map_err(query_error)?
    .is_empty();
    let mut source_columns = vec!["id", "name"];
    let retired = if columns.contains("retired_at") {
        source_columns.push("retired_at");
        "a.retired_at"
    } else {
        "NULL"
    };
    let task = if columns.contains("task_description") {
        source_columns.push("task_description");
        "COALESCE(a.task_description, '')"
    } else {
        "''"
    };
    let (ledger_join, deregistered) = if has_ledger {
        (
            "LEFT JOIN agent_deregistrations d ON d.agent_id = a.id",
            "d.agent_id",
        )
    } else {
        ("", "NULL")
    };
    let source_projection = source_columns.join(", ");
    let sql = format!(
        "SELECT a.id AS id, a.name AS name, {retired} AS retired_at, \
         {deregistered} AS deregistered_id, {task} AS task_description \
         FROM (SELECT {source_projection} FROM agents \
               WHERE {source_filter}) a \
         {ledger_join} ORDER BY a.id ASC"
    );
    let rows = query(&sql, params).map_err(query_error)?;
    let mut seen_names = HashSet::new();
    let mut agents = Vec::with_capacity(rows.len());
    for row in rows {
        let name = row.get_named::<String>("name").map_err(query_error)?;
        if !seen_names.insert(name.to_ascii_lowercase()) {
            continue;
        }
        let task = row
            .get_named::<String>("task_description")
            .map_err(query_error)?;
        let status = if row.get_by_name("deregistered_id") != Some(&Value::Null)
            || mcp_agent_mail_db::models::task_description_uses_reserved_deregistered_prefix(&task)
        {
            ComposeAgentStatus::Deregistered
        } else if row.get_by_name("retired_at") != Some(&Value::Null) {
            ComposeAgentStatus::Retired
        } else {
            ComposeAgentStatus::Active
        };
        agents.push(ComposeAgent {
            id: row.get_named("id").map_err(query_error)?,
            name,
            status,
        });
    }
    Ok(agents)
}

pub fn active_compose_agent_names(agents: &[ComposeAgent]) -> Vec<String> {
    let mut names: Vec<_> = agents
        .iter()
        .filter(|agent| agent.status == ComposeAgentStatus::Active)
        .map(|agent| agent.name.clone())
        .collect();
    names.sort();
    names
}

pub fn require_active_compose_recipient(agents: &[ComposeAgent], name: &str) -> Result<(), String> {
    agents
        .iter()
        .find(|agent| agent.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("Unknown recipient: {name}"))?
        .require_active()
}

pub fn open_live_recipient_connection(
    config: &Config,
) -> Result<mcp_agent_mail_db::GuardedReadOnlyConn, String> {
    let sqlite_path = crate::resolve_server_database_url_sqlite_path(&config.database_url)
        .ok_or_else(|| "Database connection unavailable".to_string())?;
    crate::open_read_only_sync_db_connection_with_busy_timeout(
        sqlite_path.to_string_lossy().as_ref(),
        crate::INTERACTIVE_SYNC_DB_BUSY_TIMEOUT_MS,
        "message recipient lifecycle",
    )
    .map_err(|error| format!("Cannot verify current agent lifecycle: {error}"))
}

pub fn open_live_mutation_db_connection(config: &Config) -> Result<DbConn, String> {
    let sqlite_path = crate::resolve_server_database_url_sqlite_path(&config.database_url)
        .ok_or_else(|| "Database connection unavailable".to_string())?;
    if !sqlite_path.exists() {
        return Err("Database connection unavailable".to_string());
    }
    crate::open_interactive_sync_db_connection(sqlite_path.to_string_lossy().as_ref())
        .map_err(|error| format!("Database connection unavailable: {error}"))
}

/// The caller holds the recovery write lease and has verified the project and
/// recipients before entering this helper. Existing profiles are never replaced
/// by a compose heartbeat, and retired/deregistered identities stay inactive.
pub fn ensure_compose_sender_on_connection(
    conn: &DbConn,
    config: &Config,
    project_id: i64,
    project_slug: &str,
) -> Result<(), String> {
    let existing_sender = query_compose_agent_rows(
        |sql, params| conn.query_sync(sql, params),
        "project_id = ? AND name = ? COLLATE NOCASE",
        &[
            Value::BigInt(project_id),
            Value::Text(OVERSEER_AGENT_NAME.to_string()),
        ],
    )?
    .into_iter()
    .next();
    let now = mcp_agent_mail_core::timestamps::now_micros();
    if let Some(sender) = existing_sender {
        sender.require_active()?;
        let changed = conn
            .execute_sync(
                "UPDATE agents SET last_active_ts = ? WHERE id = ? \
                 AND retired_at IS NULL \
                 AND id NOT IN (SELECT agent_id FROM agent_deregistrations) \
                 AND substr(COALESCE(task_description, ''), 1, 14) <> '[DEREGISTERED '",
                &[Value::BigInt(now), Value::BigInt(sender.id)],
            )
            .map_err(|error| format!("Failed to update compose sender activity: {error}"))?;
        if changed != 1 {
            return Err(format!(
                "Agent '{}' changed lifecycle while composing. Refresh its status before sending.",
                sender.name
            ));
        }
        return Ok(());
    }
    conn.execute_sync(
        "INSERT INTO agents \
         (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(project_id),
            Value::Text(OVERSEER_AGENT_NAME.to_string()),
            Value::Text("am-tui".to_string()),
            Value::Text("human".to_string()),
            Value::Text("Human operator composing messages in TUI".to_string()),
            Value::BigInt(now),
            Value::BigInt(now),
            Value::Text("auto".to_string()),
            Value::Text("auto".to_string()),
        ],
    )
    .map_err(|error| format!("Failed to create compose sender: {error}"))?;

    if let Ok(Some(archive)) = mcp_agent_mail_storage::open_archive(config, project_slug) {
        let agent_json = serde_json::json!({
            "name": OVERSEER_AGENT_NAME,
            "program": "am-tui",
            "model": "human",
            "task_description": "Human operator composing messages in TUI",
            "inception_ts": micros_to_iso(now),
            "last_active_ts": micros_to_iso(now),
            "attachments_policy": "auto",
            "contact_policy": "auto",
            "reaper_exempt": false,
            "retired_at": null,
            "deregistered_at": null,
        });
        let _ =
            mcp_agent_mail_storage::write_agent_profile_with_config(&archive, config, &agent_json);
    }
    Ok(())
}

pub fn verify_form_mailbox_source(
    config: &Config,
    displayed_source: Option<&(String, PathBuf)>,
) -> Result<(), String> {
    // Send/reply use the cached tool configuration. Re-reading only the
    // environment here could approve a mailbox the tools have not adopted.
    let current = Config::get();
    let captured_path = crate::resolve_server_database_url_sqlite_path(&config.database_url);
    let current_path = crate::resolve_server_database_url_sqlite_path(&current.database_url);
    let source_matches = displayed_source.is_none_or(|(url, root)| {
        crate::resolve_server_database_url_sqlite_path(url) == captured_path
            && root == &config.storage_root
    });
    if captured_path.is_none()
        || captured_path != current_path
        || config.storage_root != current.storage_root
        || !source_matches
    {
        return Err(
            "This draft belongs to a different mailbox than the sending tools; refresh with the same database and storage configuration before sending".to_string(),
        );
    }
    Ok(())
}

fn require_compose_mailbox_source(
    config: &Config,
    displayed_source: Option<&(String, PathBuf)>,
) -> Result<(), String> {
    if displayed_source.is_none() {
        return Err("The draft's mailbox is unknown. Reopen compose before sending.".to_string());
    }
    verify_form_mailbox_source(config, displayed_source)
}

fn verify_compose_project(
    mut query: impl FnMut(&str, &[Value]) -> Result<Vec<Row>, SqlError>,
    project: &ComposeProject,
) -> Result<(), String> {
    let rows = query(
        "SELECT id FROM projects WHERE id = ? AND slug = ? AND human_key = ? LIMIT 1",
        &[
            Value::BigInt(project.id),
            Value::Text(project.slug.clone()),
            Value::Text(project.human_key.clone()),
        ],
    )
    .map_err(|error| format!("Cannot verify compose project: {error}"))?;
    if rows.is_empty() {
        return Err(format!(
            "Project '{}' changed or is unavailable. Reopen compose and select its current project.",
            project.slug
        ));
    }
    Ok(())
}

/// Read all real projects from the captured live mailbox, independent of the
/// event ring. The UI must select a concrete project before asking for a roster.
pub fn load_projects(
    config: &Config,
    displayed_source: Option<&(String, PathBuf)>,
) -> Result<Vec<ComposeProject>, String> {
    require_compose_mailbox_source(config, displayed_source)?;
    let conn = open_live_recipient_connection(config)?;
    let rows = conn
        .query_sync(
            "SELECT id, slug, human_key FROM projects ORDER BY slug, id",
            &[],
        )
        .map_err(|error| format!("Cannot load compose projects: {error}"))?;
    rows.into_iter()
        .map(|row| {
            Ok(ComposeProject {
                id: row.get_named("id")?,
                slug: row.get_named("slug")?,
                human_key: row.get_named("human_key")?,
            })
        })
        .collect::<Result<_, SqlError>>()
        .map_err(|error| format!("Cannot load compose projects: {error}"))
}

pub fn load_recipients(
    config: &Config,
    displayed_source: Option<&(String, PathBuf)>,
    project: &ComposeProject,
) -> Result<Vec<String>, String> {
    require_compose_mailbox_source(config, displayed_source)?;
    let conn = open_live_recipient_connection(config)?;
    verify_compose_project(|sql, params| conn.query_sync(sql, params), project)?;
    let agents = query_compose_agent_rows(
        |sql, params| conn.query_sync(sql, params),
        "project_id = ?",
        &[Value::BigInt(project.id)],
    )?;
    Ok(active_compose_agent_names(&agents))
}

/// Called on an operator worker, never on the UI or server supervisor thread.
/// Reusing the idempotency key with the same envelope makes uncertain sends safe
/// to retry without duplicating the message or its archive files.
pub fn send_compose(
    config: &Config,
    displayed_source: Option<&(String, PathBuf)>,
    envelope: ComposeEnvelope,
    idempotency_key: String,
) -> Result<OperatorTaskResult, String> {
    require_compose_mailbox_source(config, displayed_source)?;
    if envelope.sender_name != OVERSEER_AGENT_NAME {
        return Err("Operator messages must use the HumanOverseer identity".to_string());
    }
    if envelope.to.is_empty() && envelope.cc.is_empty() && envelope.bcc.is_empty() {
        return Err("Select at least one recipient".to_string());
    }
    if envelope.subject.trim().is_empty() || envelope.body_md.trim().is_empty() {
        return Err("A subject and message body are required".to_string());
    }
    if idempotency_key.trim().is_empty() {
        return Err("This draft has no delivery key. Reopen compose before sending.".to_string());
    }
    {
        // Freeze recovery promotion before opening the live fd. Validate the
        // entire envelope before the first sender or archive mutation.
        let _write_activity = mcp_agent_mail_db::write_barrier::begin_write_activity();
        let conn = open_live_mutation_db_connection(config)?;
        verify_compose_project(
            |sql, params| conn.query_sync(sql, params),
            &envelope.project,
        )?;
        let agents = query_compose_agent_rows(
            |sql, params| conn.query_sync(sql, params),
            "project_id = ?",
            &[Value::BigInt(envelope.project.id)],
        )?;
        for name in envelope.to.iter().chain(&envelope.cc).chain(&envelope.bcc) {
            require_active_compose_recipient(&agents, name)?;
        }
        ensure_compose_sender_on_connection(
            &conn,
            config,
            envelope.project.id,
            &envelope.project.slug,
        )?;
    }

    let summary = format!("Message sent in project {}", envelope.project.slug);
    let response = block_on(mcp_agent_mail_tools::messaging::send_message(
        &operator_tool_context(),
        envelope.project.slug,
        envelope.sender_name,
        envelope.to,
        envelope.subject,
        envelope.body_md,
        (!envelope.cc.is_empty()).then_some(envelope.cc),
        (!envelope.bcc.is_empty()).then_some(envelope.bcc),
        None,
        None,
        Some(envelope.importance),
        Some(envelope.ack_required),
        envelope.thread_id,
        None,
        None,
        None,
        None,
        Some(idempotency_key),
    ))
    .map_err(|error| error.to_string())?;
    let data = serde_json::from_str(&response).map_err(|error| {
        format!("Cannot read delivery receipt; retry this draft safely: {error}")
    })?;
    Ok(OperatorTaskResult { summary, data })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::tui_screens::{
        seed_operator_test_project, with_operator_test_conn, with_operator_test_mailbox,
    };

    fn source(config: &Config) -> (String, PathBuf) {
        (config.database_url.clone(), config.storage_root.clone())
    }

    fn draft(project: &ComposeProject) -> ComposeEnvelope {
        ComposeEnvelope {
            project: project.clone(),
            sender_name: OVERSEER_AGENT_NAME.to_string(),
            to: vec!["BlueLake".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Operator follow-up".to_string(),
            body_md: "Complete\noperator message".to_string(),
            importance: "normal".to_string(),
            thread_id: None,
            ack_required: false,
        }
    }

    fn archive_files(directory: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(directory).expect("archive directory") {
            let entry = entry.expect("archive entry");
            if entry.file_type().expect("archive file type").is_dir() {
                files.extend(archive_files(&entry.path()));
            } else {
                files.push(entry.path());
            }
        }
        files
    }

    fn archived_message(directory: &Path, message_id: i64) -> (serde_json::Value, String) {
        let mut messages: Vec<_> = archive_files(directory)
            .into_iter()
            .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
            .filter_map(|path| mcp_agent_mail_storage::read_message_file(&path).ok())
            .filter(|(frontmatter, _)| frontmatter["id"].as_i64() == Some(message_id))
            .collect();
        assert_eq!(
            messages.len(),
            1,
            "one archived copy of message #{message_id}"
        );
        messages.pop().unwrap()
    }

    #[test]
    fn global_compose_uses_live_roster_and_selected_project_with_normal_archives() {
        with_operator_test_mailbox(|project_key| {
            let first_slug =
                seed_operator_test_project(project_key, &["BlueLake", "RedFox", "GreenCastle"]);
            let second_root = Path::new(project_key).with_file_name("second-project");
            std::fs::create_dir_all(&second_root).unwrap();
            let second_slug = seed_operator_test_project(
                second_root.to_str().unwrap(),
                &["BlueLake", "RedFox", "GreenCastle"],
            );
            let config = Config::from_env();
            let displayed_source = source(&config);
            // No UI event history is involved: existing registrations in both
            // projects are discoverable when compose first opens.
            let projects = load_projects(&config, Some(&displayed_source)).unwrap();
            assert_eq!(projects.len(), 2);
            let first = projects.iter().find(|p| p.slug == first_slug).unwrap();
            let selected = projects.iter().find(|p| p.slug == second_slug).unwrap();
            assert_ne!(first.id, selected.id);
            assert_eq!(
                load_recipients(&config, Some(&displayed_source), selected).unwrap(),
                ["BlueLake", "GreenCastle", "RedFox"]
            );

            let mut envelope = draft(selected);
            envelope.to = vec!["bluelake".to_string()];
            envelope.cc = vec!["redfox".to_string()];
            envelope.bcc = vec!["greencastle".to_string()];
            envelope.importance = "urgent".to_string();
            envelope.ack_required = true;
            envelope.thread_id = Some("operator-global-thread".to_string());
            let sent = send_compose(
                &config,
                Some(&displayed_source),
                envelope.clone(),
                "global-compose-project-send".to_string(),
            )
            .unwrap();
            let message_id = sent.data["deliveries"][0]["payload"]["id"]
                .as_i64()
                .unwrap();
            let replay = send_compose(
                &config,
                Some(&displayed_source),
                envelope.clone(),
                "global-compose-project-send".to_string(),
            )
            .unwrap();
            assert_eq!(replay.data["idempotent_replay"], true);
            assert_eq!(
                replay.data["deliveries"][0]["payload"]["id"].as_i64(),
                Some(message_id)
            );

            with_operator_test_conn(|conn| {
                let messages = conn
                    .query_sync(
                        "SELECT m.id, m.project_id, m.subject, m.body_md, m.thread_id, \
                                m.importance, m.ack_required, a.name AS sender_name \
                         FROM messages m JOIN agents a ON a.id = m.sender_id",
                        &[],
                    )
                    .unwrap();
                assert_eq!(messages.len(), 1, "retry must not duplicate delivery");
                let message = &messages[0];
                assert_eq!(message.get_named::<i64>("id").unwrap(), message_id);
                assert_eq!(message.get_named::<i64>("project_id").unwrap(), selected.id);
                assert_eq!(
                    message.get_named::<String>("subject").unwrap(),
                    envelope.subject
                );
                assert_eq!(
                    message.get_named::<String>("body_md").unwrap(),
                    envelope.body_md
                );
                assert_eq!(message.get_named::<String>("importance").unwrap(), "urgent");
                assert_eq!(message.get_named::<i64>("ack_required").unwrap(), 1);
                assert_eq!(
                    message.get_named::<String>("thread_id").unwrap(),
                    "operator-global-thread"
                );
                assert_eq!(
                    message.get_named::<String>("sender_name").unwrap(),
                    OVERSEER_AGENT_NAME
                );
                let rows = conn
                    .query_sync(
                        "SELECT a.project_id, a.name, r.kind FROM message_recipients r \
                         JOIN agents a ON a.id = r.agent_id \
                         WHERE r.message_id = ? ORDER BY a.name",
                        &[Value::BigInt(message_id)],
                    )
                    .unwrap();
                let recipients: Vec<_> = rows
                    .iter()
                    .map(|row| {
                        (
                            row.get_named::<i64>("project_id").unwrap(),
                            row.get_named::<String>("name").unwrap(),
                            row.get_named::<String>("kind").unwrap(),
                        )
                    })
                    .collect();
                assert_eq!(
                    recipients,
                    [
                        (selected.id, "BlueLake".to_string(), "to".to_string()),
                        (selected.id, "GreenCastle".to_string(), "bcc".to_string()),
                        (selected.id, "RedFox".to_string(), "cc".to_string()),
                    ]
                );
                assert!(
                    conn.query_sync(
                        "SELECT id FROM agents WHERE project_id = ? AND name = ?",
                        &[
                            Value::BigInt(first.id),
                            Value::Text(OVERSEER_AGENT_NAME.to_string()),
                        ],
                    )
                    .unwrap()
                    .is_empty(),
                    "no operator identity created in a different project"
                );
            });

            // Delivery schedules the normal archive write path. Drain its
            // queues deterministically before inspecting durable copies.
            assert!(
                mcp_agent_mail_storage::archive_backlog_flush_blocking(
                    std::time::Duration::from_secs(15)
                ),
                "archive retry backlog drains"
            );
            mcp_agent_mail_storage::wbq_flush();
            mcp_agent_mail_storage::flush_async_commits();
            let archive = mcp_agent_mail_storage::open_archive(&config, &selected.slug)
                .unwrap()
                .unwrap();
            let (canonical, body) = archived_message(&archive.root.join("messages"), message_id);
            assert_eq!(body, envelope.body_md);
            assert_eq!(canonical["to"], serde_json::json!(["BlueLake"]));
            assert_eq!(canonical["cc"], serde_json::json!(["RedFox"]));
            assert_eq!(canonical["bcc"], serde_json::json!(["GreenCastle"]));
            assert_eq!(canonical["ack_required"], true);
            assert_eq!(canonical["thread_id"], "operator-global-thread");
            let (outbox, _) = archived_message(
                &archive.root.join("agents/HumanOverseer/outbox"),
                message_id,
            );
            assert_eq!(outbox["bcc"], serde_json::json!(["GreenCastle"]));
            for name in ["BlueLake", "RedFox", "GreenCastle"] {
                let (inbox, body) = archived_message(
                    &archive.root.join("agents").join(name).join("inbox"),
                    message_id,
                );
                assert_eq!(body, envelope.body_md);
                assert_eq!(inbox["bcc"], serde_json::json!([]));
            }
        });
    }

    #[test]
    fn global_compose_refuses_stale_scope_source_and_recipients_before_sender_mutation() {
        with_operator_test_mailbox(|project_key| {
            seed_operator_test_project(
                project_key,
                &["BlueLake", "RedFox", "GreenCastle", "GoldFox"],
            );
            let config = Config::from_env();
            let displayed_source = source(&config);
            let project = load_projects(&config, Some(&displayed_source))
                .unwrap()
                .remove(0);
            let initially_active =
                load_recipients(&config, Some(&displayed_source), &project).unwrap();
            assert_eq!(initially_active.len(), 4);
            with_operator_test_conn(|conn| {
                conn.execute_sync(
                    "UPDATE agents SET retired_at = 123 WHERE name = 'RedFox'",
                    &[],
                )
                .unwrap();
                conn.execute_sync(
                    "INSERT INTO agent_deregistrations (agent_id, deregistered_at) \
                     SELECT id, 123 FROM agents WHERE name = 'GreenCastle'",
                    &[],
                )
                .unwrap();
                conn.execute_sync(
                    "UPDATE agents SET task_description = '[DEREGISTERED 2026-01-01] closed' \
                     WHERE name = 'GoldFox'",
                    &[],
                )
                .unwrap();
            });
            assert_eq!(
                load_recipients(&config, Some(&displayed_source), &project).unwrap(),
                ["BlueLake"]
            );

            let base = draft(&project);
            for (kind, name, expected) in [
                ("to", "RedFox", "retired"),
                ("cc", "GreenCastle", "deregistered"),
                ("bcc", "GoldFox", "deregistered"),
                ("cc", "MissingAgent", "Unknown recipient"),
                ("bcc", "BlueLake@another-project", "Unknown recipient"),
            ] {
                let mut envelope = base.clone();
                match kind {
                    "to" => envelope.to = vec![name.to_string()],
                    "cc" => envelope.cc = vec![name.to_string()],
                    "bcc" => envelope.bcc = vec![name.to_string()],
                    _ => unreachable!(),
                }
                let error = send_compose(
                    &config,
                    Some(&displayed_source),
                    envelope,
                    format!("refused-{kind}-{name}"),
                )
                .unwrap_err();
                assert!(error.contains(expected), "{error}");
            }

            for changed_field in ["id", "slug", "human_key"] {
                let mut stale = base.clone();
                match changed_field {
                    "id" => stale.project.id += 1000,
                    "slug" => stale.project.slug.push_str("-replaced"),
                    "human_key" => stale.project.human_key.push_str("-replaced"),
                    _ => unreachable!(),
                }
                assert!(load_recipients(&config, Some(&displayed_source), &stale.project).is_err());
                let error = send_compose(
                    &config,
                    Some(&displayed_source),
                    stale,
                    format!("stale-{changed_field}"),
                )
                .unwrap_err();
                assert!(error.contains("changed or is unavailable"), "{error}");
            }

            assert!(load_projects(&config, None).is_err());
            assert!(send_compose(&config, None, base.clone(), "unknown-source".into()).is_err());
            for wrong_source in [
                (
                    "sqlite:///unrelated.sqlite3".to_string(),
                    config.storage_root.clone(),
                ),
                (
                    config.database_url.clone(),
                    config.storage_root.join("unrelated"),
                ),
            ] {
                assert!(load_projects(&config, Some(&wrong_source)).is_err());
                let error = send_compose(
                    &config,
                    Some(&wrong_source),
                    base.clone(),
                    "wrong-source".to_string(),
                )
                .unwrap_err();
                assert!(error.contains("different mailbox"), "{error}");
            }
            let mut wrong_config = Config::from_env();
            wrong_config.storage_root = config.storage_root.join("not-the-tool-mailbox");
            let wrong_source = source(&wrong_config);
            let error = send_compose(
                &wrong_config,
                Some(&wrong_source),
                base,
                "changed-config".to_string(),
            )
            .unwrap_err();
            assert!(error.contains("different mailbox"), "{error}");
            with_operator_test_conn(|conn| {
                assert!(
                    conn.query_sync("SELECT id FROM messages", &[])
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    conn.query_sync(
                        "SELECT id FROM agents \
                         WHERE name = 'HumanOverseer' OR name = 'MissingAgent'",
                        &[],
                    )
                    .unwrap()
                    .is_empty(),
                    "rejected drafts create no sender or placeholder recipient"
                );
            });
            let archive = mcp_agent_mail_storage::open_archive(&config, &project.slug)
                .unwrap()
                .unwrap();
            assert!(!archive.root.join("agents/HumanOverseer").exists());
        });
    }

    #[test]
    fn global_compose_supports_cc_only_and_bcc_only_without_synthesizing_threads() {
        with_operator_test_mailbox(|project_key| {
            seed_operator_test_project(project_key, &["BlueLake"]);
            let config = Config::from_env();
            let displayed_source = source(&config);
            let project = load_projects(&config, Some(&displayed_source))
                .unwrap()
                .remove(0);
            for kind in ["cc", "bcc"] {
                let mut envelope = draft(&project);
                envelope.to.clear();
                if kind == "cc" {
                    envelope.cc.push("BlueLake".to_string());
                } else {
                    envelope.bcc.push("BlueLake".to_string());
                }
                let result = send_compose(
                    &config,
                    Some(&displayed_source),
                    envelope,
                    format!("global-compose-{kind}-only"),
                )
                .unwrap();
                let message_id = result.data["deliveries"][0]["payload"]["id"]
                    .as_i64()
                    .unwrap();
                with_operator_test_conn(|conn| {
                    let rows = conn
                        .query_sync(
                            "SELECT m.thread_id, r.kind FROM messages m \
                             JOIN message_recipients r ON r.message_id = m.id WHERE m.id = ?",
                            &[Value::BigInt(message_id)],
                        )
                        .unwrap();
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0].get_by_name("thread_id"), Some(&Value::Null));
                    assert_eq!(rows[0].get_named::<String>("kind").unwrap(), kind);
                });
            }
        });
    }
}
