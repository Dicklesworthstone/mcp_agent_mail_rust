//! Live mailbox and delivery operations shared by the operator compose forms.
//!
//! Drafts identify both their mailbox and their project. Recipient suggestions
//! are advisory: delivery rechecks the live roster before creating or touching
//! the operator identity, then uses the normal messaging tool and archive path.

use std::collections::HashSet;
use std::path::PathBuf;

use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::pool::SyncQuery;
use mcp_agent_mail_db::sqlmodel_core::{Error as SqlError, Row, Value};
use mcp_agent_mail_db::timestamps::micros_to_iso;

use crate::tui_compose::OVERSEER_AGENT_NAME;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposeAgentStatus {
    Active,
    Retired,
    Deregistered,
}

#[derive(Debug)]
pub(crate) struct ComposeAgent {
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
pub(crate) fn query_compose_agent_rows(
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

pub(crate) fn active_compose_agent_names(agents: &[ComposeAgent]) -> Vec<String> {
    let mut names: Vec<_> = agents
        .iter()
        .filter(|agent| agent.status == ComposeAgentStatus::Active)
        .map(|agent| agent.name.clone())
        .collect();
    names.sort();
    names
}

pub(crate) fn require_active_compose_recipient(
    agents: &[ComposeAgent],
    name: &str,
) -> Result<(), String> {
    agents
        .iter()
        .find(|agent| agent.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("Unknown recipient: {name}"))?
        .require_active()
}

pub(crate) fn open_live_recipient_connection(
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

pub(crate) fn open_live_mutation_db_connection(config: &Config) -> Result<DbConn, String> {
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
pub(crate) fn ensure_compose_sender_on_connection(
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

pub(crate) fn verify_form_mailbox_source(
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

