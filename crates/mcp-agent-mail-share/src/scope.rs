//! Step 2: Project scoping — delete rows for non-selected projects.
//!
//! Given a snapshot database and a list of project identifiers (slugs or
//! human_keys), removes all data belonging to non-selected projects.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use mcp_agent_mail_db::DbConn;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlmodel_core::Value;

use crate::ShareError;

type Conn = DbConn;

/// A project record from the `projects` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
}

/// Result of applying project scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectScopeResult {
    /// The identifiers that were requested (echoed back).
    pub identifiers: Vec<String>,
    /// Projects that matched the identifiers (kept).
    pub projects: Vec<ProjectRecord>,
    /// How many projects were removed.
    pub removed_count: usize,
    /// Remaining row counts per table after scoping.
    pub remaining: RemainingCounts,
}

/// Row counts in the scoped database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemainingCounts {
    pub projects: i64,
    pub agents: i64,
    pub messages: i64,
    pub recipients: i64,
    pub file_reservations: i64,
    pub inbox_stats: i64,
    pub agent_links: i64,
    pub project_sibling_suggestions: i64,
}

/// Apply project scoping to a snapshot database.
///
/// If `identifiers` is empty, all projects are kept and no deletions occur.
/// Otherwise, only projects matching the given slugs or human_keys (case-insensitive,
/// trimmed) are retained; all other project data is deleted.
///
/// # Errors
///
/// - [`ShareError::ScopeNoProjects`] if the database has no discoverable project identities.
/// - [`ShareError::ScopeIdentifierNotFound`] if any identifier doesn't match.
/// - [`ShareError::Sqlite`] on any SQLite error.
pub fn apply_project_scope(
    snapshot_path: &Path,
    identifiers: &[String],
) -> Result<ProjectScopeResult, ShareError> {
    let snapshot_path = crate::require_real_share_sqlite_path(snapshot_path)?;
    let path_str = snapshot_path.display().to_string();
    let conn = Conn::open_file(&path_str).map_err(|e| ShareError::Sqlite {
        message: format!("cannot open snapshot {path_str}: {e}"),
    })?;

    // Enable foreign keys
    conn.execute_raw("PRAGMA foreign_keys = ON")
        .map_err(|e| ShareError::Sqlite {
            message: format!("PRAGMA foreign_keys failed: {e}"),
        })?;

    let all_projects = load_scope_projects(&conn)?;
    if all_projects.is_empty() {
        return Err(ShareError::ScopeNoProjects);
    }

    // If no identifiers, keep everything
    if identifiers.is_empty() {
        let remaining = count_remaining(&conn)?;
        return Ok(ProjectScopeResult {
            identifiers: Vec::new(),
            projects: all_projects,
            removed_count: 0,
            remaining,
        });
    }

    // Build lookup: slug.lower() -> record, human_key.lower() -> record
    let mut lookup: HashMap<String, &ProjectRecord> = HashMap::new();
    for p in &all_projects {
        lookup.insert(p.slug.to_ascii_lowercase(), p);
        lookup.insert(p.human_key.to_ascii_lowercase(), p);
    }

    // Match identifiers (case-insensitive, trimmed)
    let mut matched: Vec<ProjectRecord> = Vec::new();
    let mut matched_ids: Vec<i64> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for ident in identifiers {
        let key = ident.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        match lookup.get(&key) {
            Some(p) => {
                if seen_ids.insert(p.id) {
                    matched.push((*p).clone());
                    matched_ids.push(p.id);
                }
            }
            None => {
                return Err(ShareError::ScopeIdentifierNotFound {
                    identifier: ident.clone(),
                });
            }
        }
    }

    // If all provided identifiers were empty/whitespace, treat as no-op scope.
    if matched_ids.is_empty() {
        let remaining = count_remaining(&conn)?;
        return Ok(ProjectScopeResult {
            identifiers: identifiers.to_vec(),
            projects: all_projects,
            removed_count: 0,
            remaining,
        });
    }

    // Compute disallowed IDs
    let all_ids: Vec<i64> = all_projects.iter().map(|p| p.id).collect();
    let disallowed: Vec<i64> = all_ids
        .iter()
        .filter(|id| !seen_ids.contains(id))
        .copied()
        .collect();
    let removed_count = disallowed.len();

    // If nothing to remove, return early
    if disallowed.is_empty() {
        let remaining = count_remaining(&conn)?;
        return Ok(ProjectScopeResult {
            identifiers: identifiers.to_vec(),
            projects: matched,
            removed_count: 0,
            remaining,
        });
    }

    let derived_artifacts = crate::scrub::detect_derived_export_artifacts(&conn)?;

    // Build SQL placeholders for allowed IDs
    let placeholders = build_placeholders(matched_ids.len());
    let id_values: Vec<Value> = matched_ids.iter().map(|&id| Value::BigInt(id)).collect();

    conn.execute_sync("BEGIN IMMEDIATE", &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("BEGIN transaction failed: {e}"),
        })?;

    let result = (|| {
        // Delete order — use NOT IN (allowed_ids) for safety
        // 1. agent_links (cross-project)
        if table_exists(&conn, "agent_links")? {
            let sql = format!(
                "DELETE FROM agent_links WHERE a_project_id NOT IN ({p}) OR b_project_id NOT IN ({p})",
                p = placeholders
            );
            let mut params = id_values.clone();
            params.extend(id_values.iter().cloned());
            exec(&conn, &sql, &params)?;
        }

        // 2. project_sibling_suggestions
        if table_exists(&conn, "project_sibling_suggestions")? {
            let sql = format!(
                "DELETE FROM project_sibling_suggestions WHERE project_a_id NOT IN ({p}) OR project_b_id NOT IN ({p})",
                p = placeholders
            );
            let mut params = id_values.clone();
            params.extend(id_values.iter().cloned());
            exec(&conn, &sql, &params)?;
        }

        // 3. Collect message IDs for non-allowed projects
        let msg_sql = format!(
            "SELECT id FROM messages WHERE project_id NOT IN ({p}) ORDER BY id ASC",
            p = placeholders
        );
        let msg_rows = conn
            .query_sync(&msg_sql, &id_values)
            .map_err(|e| ShareError::Sqlite {
                message: format!("SELECT messages failed: {e}"),
            })?;
        let msg_ids: Vec<i64> = msg_rows
            .iter()
            .filter_map(|r| r.get_named::<i64>("id").ok())
            .collect();

        // 4. Delete message_recipients for collected message IDs
        if !msg_ids.is_empty() {
            let msg_placeholders = build_placeholders(msg_ids.len());
            let msg_values: Vec<Value> = msg_ids.iter().map(|&id| Value::BigInt(id)).collect();
            exec(
                &conn,
                &format!("DELETE FROM message_recipients WHERE message_id IN ({msg_placeholders})"),
                &msg_values,
            )?;
        }

        // 5. Delete messages
        exec(
            &conn,
            &format!(
                "DELETE FROM messages WHERE project_id NOT IN ({p})",
                p = placeholders
            ),
            &id_values,
        )?;

        // 6. Delete file_reservations
        exec(
            &conn,
            &format!(
                "DELETE FROM file_reservations WHERE project_id NOT IN ({p})",
                p = placeholders
            ),
            &id_values,
        )?;
        if table_exists(&conn, "file_reservation_releases")? {
            exec(
                &conn,
                "DELETE FROM file_reservation_releases \
                 WHERE reservation_id NOT IN (SELECT id FROM file_reservations)",
                &[],
            )?;
        }

        // 7. Delete product links scoped to filtered-out projects and then
        // remove any products that are no longer referenced by the kept scope.
        if table_exists(&conn, "product_project_links")? {
            exec(
                &conn,
                &format!(
                    "DELETE FROM product_project_links WHERE project_id NOT IN ({p})",
                    p = placeholders
                ),
                &id_values,
            )?;
            if table_exists(&conn, "products")? {
                exec(
                    &conn,
                    "DELETE FROM products \
                     WHERE id NOT IN (SELECT DISTINCT product_id FROM product_project_links)",
                    &[],
                )?;
            }
        }

        // 8. Delete agents
        exec(
            &conn,
            &format!(
                "DELETE FROM agents WHERE project_id NOT IN ({p})",
                p = placeholders
            ),
            &id_values,
        )?;

        // 9. Delete recipient links that now point at filtered-out agents, then
        // repair the denormalized recipients envelope for kept messages.
        exec(
            &conn,
            "DELETE FROM message_recipients \
             WHERE agent_id NOT IN (SELECT id FROM agents)",
            &[],
        )?;
        if column_exists(&conn, "messages", "recipients_json")? {
            sync_scope_recipients_json(&conn)?;
        }

        // 10. Rebuild agent-scoped aggregates that are not protected by FK
        // constraints. Kept agents can otherwise retain stale counts from
        // messages trimmed out of scope.
        if table_exists(&conn, "inbox_stats")? {
            rebuild_scope_inbox_stats(&conn)?;
        }

        // 11. Delete projects
        exec(
            &conn,
            &format!(
                "DELETE FROM projects WHERE id NOT IN ({placeholders})",
                placeholders = placeholders
            ),
            &id_values,
        )?;

        let remaining = count_remaining(&conn)?;

        Ok(ProjectScopeResult {
            identifiers: identifiers.to_vec(),
            projects: matched,
            removed_count,
            remaining,
        })
    })();

    match result {
        Ok(out) => {
            conn.execute_sync("COMMIT", &[])
                .map_err(|e| ShareError::Sqlite {
                    message: format!("COMMIT failed: {e}"),
                })?;
            drop(conn);
            if derived_artifacts.any() {
                crate::scrub::refresh_derived_export_artifacts(&snapshot_path, derived_artifacts)?;
            }
            Ok(out)
        }
        Err(err) => {
            let _ = conn.execute_sync("ROLLBACK", &[]);
            Err(err)
        }
    }
}

fn load_scope_projects(conn: &Conn) -> Result<Vec<ProjectRecord>, ShareError> {
    let project_rows = conn
        .query_sync(
            "SELECT id, slug, human_key FROM projects ORDER BY id ASC",
            &[],
        )
        .map_err(|e| ShareError::Sqlite {
            message: format!("SELECT projects failed: {e}"),
        })?;

    let mut projects: Vec<ProjectRecord> = project_rows
        .iter()
        .map(|row| {
            let id: i64 = row.get_named("id").unwrap_or(0);
            let slug: String = row.get_named("slug").unwrap_or_default();
            let human_key: String = row.get_named("human_key").unwrap_or_default();
            ProjectRecord {
                id,
                slug,
                human_key,
            }
        })
        .collect();

    let known_ids: BTreeSet<i64> = projects.iter().map(|project| project.id).collect();
    let orphan_ids = collect_orphan_project_ids(conn, &known_ids)?;
    for id in orphan_ids {
        let placeholder = format!("[unknown-project-{id}]");
        projects.push(ProjectRecord {
            id,
            slug: placeholder.clone(),
            human_key: placeholder,
        });
    }

    projects.sort_by_key(|project| project.id);
    Ok(projects)
}

fn collect_orphan_project_ids(
    conn: &Conn,
    known_ids: &BTreeSet<i64>,
) -> Result<Vec<i64>, ShareError> {
    let mut orphan_ids = BTreeSet::new();
    collect_orphan_project_ids_from_query(
        conn,
        "SELECT DISTINCT project_id AS project_id FROM messages ORDER BY project_id",
        known_ids,
        &mut orphan_ids,
    )?;
    collect_orphan_project_ids_from_query(
        conn,
        "SELECT DISTINCT project_id AS project_id FROM agents ORDER BY project_id",
        known_ids,
        &mut orphan_ids,
    )?;
    collect_orphan_project_ids_from_query(
        conn,
        "SELECT DISTINCT project_id AS project_id FROM file_reservations ORDER BY project_id",
        known_ids,
        &mut orphan_ids,
    )?;
    if table_exists(conn, "product_project_links")? {
        collect_orphan_project_ids_from_query(
            conn,
            "SELECT DISTINCT project_id AS project_id FROM product_project_links ORDER BY project_id",
            known_ids,
            &mut orphan_ids,
        )?;
    }
    if table_exists(conn, "agent_links")? {
        collect_orphan_project_ids_from_query(
            conn,
            "SELECT DISTINCT a_project_id AS project_id FROM agent_links ORDER BY a_project_id",
            known_ids,
            &mut orphan_ids,
        )?;
        collect_orphan_project_ids_from_query(
            conn,
            "SELECT DISTINCT b_project_id AS project_id FROM agent_links ORDER BY b_project_id",
            known_ids,
            &mut orphan_ids,
        )?;
    }
    if table_exists(conn, "project_sibling_suggestions")? {
        collect_orphan_project_ids_from_query(
            conn,
            "SELECT DISTINCT project_a_id AS project_id FROM project_sibling_suggestions ORDER BY project_a_id",
            known_ids,
            &mut orphan_ids,
        )?;
        collect_orphan_project_ids_from_query(
            conn,
            "SELECT DISTINCT project_b_id AS project_id FROM project_sibling_suggestions ORDER BY project_b_id",
            known_ids,
            &mut orphan_ids,
        )?;
    }
    Ok(orphan_ids.into_iter().collect())
}

fn collect_orphan_project_ids_from_query(
    conn: &Conn,
    sql: &str,
    known_ids: &BTreeSet<i64>,
    orphan_ids: &mut BTreeSet<i64>,
) -> Result<(), ShareError> {
    let rows = conn.query_sync(sql, &[]).map_err(|e| ShareError::Sqlite {
        message: format!("orphan project inventory query failed: {e}"),
    })?;
    for row in rows {
        if let Ok(project_id) = row.get_named::<i64>("project_id")
            && !known_ids.contains(&project_id)
        {
            orphan_ids.insert(project_id);
        }
    }
    Ok(())
}

/// Build `?,?,?` placeholder string for `n` parameters.
fn build_placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}

/// Check if a table exists in the database.
/// Uses a direct SELECT probe because FrankenConnection does not
/// support sqlite_master queries.
fn table_exists(conn: &Conn, name: &str) -> Result<bool, ShareError> {
    let probe = format!("SELECT 1 FROM \"{name}\" LIMIT 0");
    match conn.query_sync(&probe, &[]) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

fn column_exists(conn: &Conn, table: &str, column: &str) -> Result<bool, ShareError> {
    let probe = format!("SELECT \"{column}\" FROM \"{table}\" LIMIT 0");
    match conn.query_sync(&probe, &[]) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// Execute a statement with parameters, mapping errors to [`ShareError`].
fn exec(conn: &Conn, sql: &str, params: &[Value]) -> Result<u64, ShareError> {
    conn.execute_sync(sql, params)
        .map_err(|e| ShareError::Sqlite {
            message: format!("SQL exec failed: {e}"),
        })
}

fn sync_scope_recipients_json(conn: &Conn) -> Result<(), ShareError> {
    let message_rows = conn
        .query_sync("SELECT id FROM messages ORDER BY id ASC", &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("scope recipients_json message inventory failed: {e}"),
        })?;

    for row in message_rows {
        let message_id = row.get_named::<i64>("id").map_err(|e| ShareError::Sqlite {
            message: format!("scope recipients_json message id decode failed: {e}"),
        })?;
        sync_scope_message_recipients_json(conn, message_id)?;
    }

    Ok(())
}

fn sync_scope_message_recipients_json(conn: &Conn, message_id: i64) -> Result<(), ShareError> {
    let rows = conn
        .query_sync(
            "SELECT COALESCE(NULLIF(TRIM(a.name), ''), '[unknown-agent-' || mr.agent_id || ']') AS name, \
                    mr.kind AS kind \
             FROM message_recipients mr \
             LEFT JOIN agents a ON a.id = mr.agent_id \
             WHERE mr.message_id = ? \
             ORDER BY CASE mr.kind WHEN 'to' THEN 0 WHEN 'cc' THEN 1 WHEN 'bcc' THEN 2 ELSE 3 END, \
                     COALESCE(NULLIF(TRIM(a.name), ''), '[unknown-agent-' || mr.agent_id || ']') COLLATE NOCASE",
            &[Value::BigInt(message_id)],
        )
        .map_err(|e| ShareError::Sqlite {
            message: format!("scope recipients_json query failed for message {message_id}: {e}"),
        })?;

    let mut to_names = Vec::new();
    let mut cc_names = Vec::new();
    let mut bcc_names = Vec::new();

    for row in rows {
        let name = row
            .get_named::<String>("name")
            .map_err(|e| ShareError::Sqlite {
                message: format!(
                    "scope recipients_json name decode failed for message {message_id}: {e}"
                ),
            })?;
        let kind = row
            .get_named::<String>("kind")
            .map_err(|e| ShareError::Sqlite {
                message: format!(
                    "scope recipients_json kind decode failed for message {message_id}: {e}"
                ),
            })?;
        match kind.as_str() {
            "cc" => cc_names.push(name),
            "bcc" => bcc_names.push(name),
            _ => to_names.push(name),
        }
    }

    let recipients_json = json!({
        "to": to_names,
        "cc": cc_names,
        "bcc": bcc_names,
    })
    .to_string();

    conn.execute_sync(
        "UPDATE messages SET recipients_json = ? WHERE id = ?",
        &[Value::Text(recipients_json), Value::BigInt(message_id)],
    )
    .map_err(|e| ShareError::Sqlite {
        message: format!("scope recipients_json update failed for message {message_id}: {e}"),
    })?;

    Ok(())
}

fn rebuild_scope_inbox_stats(conn: &Conn) -> Result<(), ShareError> {
    conn.execute_sync("DELETE FROM inbox_stats", &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("scope inbox_stats reset failed: {e}"),
        })?;

    conn.execute_sync(
        "INSERT INTO inbox_stats (\
             agent_id, total_count, unread_count, ack_pending_count, last_message_ts\
         ) \
         SELECT \
             mr.agent_id AS agent_id, \
             COUNT(*) AS total_count, \
             SUM(CASE WHEN mr.read_ts IS NULL THEN 1 ELSE 0 END) AS unread_count, \
             SUM(CASE WHEN COALESCE(m.ack_required, 0) = 1 AND mr.ack_ts IS NULL THEN 1 ELSE 0 END) \
                 AS ack_pending_count, \
             MAX(m.created_ts) AS last_message_ts \
         FROM message_recipients mr \
         INNER JOIN messages m ON m.id = mr.message_id \
         INNER JOIN agents a ON a.id = mr.agent_id \
         GROUP BY mr.agent_id",
        &[],
    )
    .map_err(|e| ShareError::Sqlite {
        message: format!("scope inbox_stats rebuild failed: {e}"),
    })?;

    Ok(())
}

/// Count remaining rows in all relevant tables.
fn count_remaining(conn: &Conn) -> Result<RemainingCounts, ShareError> {
    Ok(RemainingCounts {
        projects: count_table(conn, "projects")?,
        agents: count_table(conn, "agents")?,
        messages: count_table(conn, "messages")?,
        recipients: count_table(conn, "message_recipients")?,
        file_reservations: count_table(conn, "file_reservations")?,
        inbox_stats: count_if_exists(conn, "inbox_stats")?,
        agent_links: count_if_exists(conn, "agent_links")?,
        project_sibling_suggestions: count_if_exists(conn, "project_sibling_suggestions")?,
    })
}

fn count_table(conn: &Conn, table: &str) -> Result<i64, ShareError> {
    // Table names cannot be bound parameters; keep a strict allowlist to avoid
    // accidental SQL injection if this helper is ever reused.
    let sql = match table {
        "projects" => "SELECT COUNT(*) AS cnt FROM projects",
        "agents" => "SELECT COUNT(*) AS cnt FROM agents",
        "messages" => "SELECT COUNT(*) AS cnt FROM messages",
        "message_recipients" => "SELECT COUNT(*) AS cnt FROM message_recipients",
        "file_reservations" => "SELECT COUNT(*) AS cnt FROM file_reservations",
        "inbox_stats" => "SELECT COUNT(*) AS cnt FROM inbox_stats",
        "agent_links" => "SELECT COUNT(*) AS cnt FROM agent_links",
        "project_sibling_suggestions" => "SELECT COUNT(*) AS cnt FROM project_sibling_suggestions",
        other => {
            return Err(ShareError::Sqlite {
                message: format!("unsupported table for COUNT(*): {other}"),
            });
        }
    };
    let rows = conn.query_sync(sql, &[]).map_err(|e| ShareError::Sqlite {
        message: format!("COUNT(*) from {table} failed: {e}"),
    })?;
    Ok(rows
        .first()
        .and_then(|r| r.get_named::<i64>("cnt").ok())
        .unwrap_or(0))
}

fn count_if_exists(conn: &Conn, table: &str) -> Result<i64, ShareError> {
    if table_exists(conn, table)? {
        count_table(conn, table)
    } else {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn create_test_db(dir: &Path) -> PathBuf {
        let db_path = dir.join("test.sqlite3");
        let conn = Conn::open_file(db_path.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT ''
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT NOT NULL DEFAULT '',
                model TEXT NOT NULL DEFAULT '',
                task_description TEXT NOT NULL DEFAULT '',
                inception_ts TEXT NOT NULL DEFAULT '',
                last_active_ts TEXT NOT NULL DEFAULT '',
                attachments_policy TEXT NOT NULL DEFAULT 'auto',
                contact_policy TEXT NOT NULL DEFAULT 'auto'
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                sender_id INTEGER NOT NULL,
                thread_id TEXT,
                subject TEXT NOT NULL DEFAULT '',
                body_md TEXT NOT NULL DEFAULT '',
                importance TEXT NOT NULL DEFAULT 'normal',
                ack_required INTEGER NOT NULL DEFAULT 0,
                created_ts TEXT NOT NULL DEFAULT '',
                recipients_json TEXT NOT NULL DEFAULT '{}',
                attachments TEXT NOT NULL DEFAULT '[]'
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (
                message_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                kind TEXT NOT NULL DEFAULT 'to',
                read_ts TEXT,
                ack_ts TEXT,
                PRIMARY KEY (message_id, agent_id)
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                path_pattern TEXT NOT NULL,
                exclusive INTEGER NOT NULL DEFAULT 1,
                reason TEXT NOT NULL DEFAULT '',
                created_ts TEXT NOT NULL DEFAULT '',
                expires_ts TEXT NOT NULL DEFAULT '',
                released_ts TEXT
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE inbox_stats (
                agent_id INTEGER PRIMARY KEY,
                total_count INTEGER NOT NULL DEFAULT 0,
                unread_count INTEGER NOT NULL DEFAULT 0,
                ack_pending_count INTEGER NOT NULL DEFAULT 0,
                last_message_ts INTEGER
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE agent_links (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                a_project_id INTEGER NOT NULL,
                a_agent_id INTEGER NOT NULL,
                b_project_id INTEGER NOT NULL,
                b_agent_id INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                reason TEXT NOT NULL DEFAULT '',
                created_ts TEXT NOT NULL DEFAULT '',
                updated_ts TEXT NOT NULL DEFAULT '',
                expires_ts TEXT
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE project_sibling_suggestions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_a_id INTEGER NOT NULL,
                project_b_id INTEGER NOT NULL,
                score REAL NOT NULL DEFAULT 0.0,
                status TEXT NOT NULL DEFAULT 'suggested',
                rationale TEXT NOT NULL DEFAULT '',
                created_ts TEXT NOT NULL DEFAULT '',
                evaluated_ts TEXT NOT NULL DEFAULT '',
                confirmed_ts TEXT,
                dismissed_ts TEXT
            )",
        )
        .unwrap();

        // Insert test data: 2 projects, 2 agents, 3 messages
        conn.execute_raw(
            "INSERT INTO projects (slug, human_key) VALUES ('proj-alpha', '/data/projects/alpha')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO projects (slug, human_key) VALUES ('proj-beta', '/data/projects/beta')",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO agents (project_id, name) VALUES (1, 'GreenCastle')")
            .unwrap();
        conn.execute_raw("INSERT INTO agents (project_id, name) VALUES (2, 'PurpleBear')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO messages (project_id, sender_id, subject) VALUES (1, 1, 'Msg A')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages (project_id, sender_id, subject) VALUES (1, 1, 'Msg B')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages (project_id, sender_id, subject) VALUES (2, 2, 'Msg C')",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO message_recipients (message_id, agent_id) VALUES (1, 1)")
            .unwrap();
        conn.execute_raw("INSERT INTO message_recipients (message_id, agent_id) VALUES (2, 1)")
            .unwrap();
        conn.execute_raw("INSERT INTO message_recipients (message_id, agent_id) VALUES (3, 2)")
            .unwrap();
        conn.execute_raw("INSERT INTO file_reservations (project_id, agent_id, path_pattern) VALUES (1, 1, 'src/*.rs')")
            .unwrap();
        conn.execute_raw("INSERT INTO inbox_stats (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) VALUES (1, 2, 1, 1, 10)")
            .unwrap();
        conn.execute_raw("INSERT INTO inbox_stats (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) VALUES (2, 1, 1, 0, 20)")
            .unwrap();
        conn.execute_raw("INSERT INTO agent_links (a_project_id, a_agent_id, b_project_id, b_agent_id) VALUES (1, 1, 2, 2)")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO project_sibling_suggestions (project_a_id, project_b_id) VALUES (1, 2)",
        )
        .unwrap();

        db_path
    }

    #[cfg(unix)]
    #[test]
    fn apply_project_scope_rejects_symlinked_snapshot() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let linked = dir.path().join("linked.sqlite3");
        symlink(&db, &linked).unwrap();

        let err = apply_project_scope(&linked, &["proj-alpha".to_string()])
            .expect_err("symlinked snapshots must fail validation");
        assert!(matches!(err, ShareError::Validation { .. }));
        assert!(err.to_string().contains("real file"));
    }

    #[test]
    fn scope_empty_identifiers_keeps_all() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let result = apply_project_scope(&db, &[]).unwrap();
        assert_eq!(result.removed_count, 0);
        assert_eq!(result.projects.len(), 2);
        assert_eq!(result.remaining.messages, 3);
    }

    #[test]
    fn scope_by_slug() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let result = apply_project_scope(&db, &["proj-alpha".to_string()]).unwrap();
        assert_eq!(result.removed_count, 1);
        assert_eq!(result.projects.len(), 1);
        assert_eq!(result.projects[0].slug, "proj-alpha");
        assert_eq!(result.remaining.projects, 1);
        assert_eq!(result.remaining.agents, 1);
        assert_eq!(result.remaining.messages, 2);
        assert_eq!(result.remaining.recipients, 2);
        assert_eq!(result.remaining.file_reservations, 1);
        assert_eq!(result.remaining.inbox_stats, 1);
        assert_eq!(result.remaining.agent_links, 0);
        assert_eq!(result.remaining.project_sibling_suggestions, 0);
    }

    #[test]
    fn scope_by_human_key_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let result = apply_project_scope(&db, &["/DATA/PROJECTS/ALPHA".to_string()]).unwrap();
        assert_eq!(result.removed_count, 1);
        assert_eq!(result.projects[0].slug, "proj-alpha");
    }

    #[test]
    fn scope_ignores_empty_identifiers() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let result = apply_project_scope(
            &db,
            &["".to_string(), "   ".to_string(), "proj-alpha".to_string()],
        )
        .unwrap();
        assert_eq!(result.removed_count, 1);
        assert_eq!(result.projects.len(), 1);
        assert_eq!(result.projects[0].slug, "proj-alpha");
    }

    #[test]
    fn scope_only_empty_identifiers_keeps_all() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let result = apply_project_scope(&db, &["".to_string(), "   ".to_string()]).unwrap();
        assert_eq!(result.removed_count, 0);
        assert_eq!(result.projects.len(), 2);
        assert_eq!(result.remaining.projects, 2);
        assert_eq!(result.remaining.messages, 3);
    }

    #[test]
    fn scope_unknown_identifier_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let result = apply_project_scope(&db, &["nonexistent".to_string()]);
        assert!(matches!(
            result,
            Err(ShareError::ScopeIdentifierNotFound { .. })
        ));
    }

    #[test]
    fn count_table_rejects_unknown_table_name() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();
        let result = count_table(&conn, "unknown_table");
        assert!(matches!(result, Err(ShareError::Sqlite { .. })));
    }

    #[test]
    fn scope_multiple_identifiers() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        // Select both projects - nothing should be removed
        let result =
            apply_project_scope(&db, &["proj-alpha".to_string(), "proj-beta".to_string()]).unwrap();
        assert_eq!(result.removed_count, 0);
        assert_eq!(result.projects.len(), 2);
        assert_eq!(result.remaining.messages, 3);
        assert_eq!(result.remaining.agents, 2);
        assert_eq!(result.remaining.inbox_stats, 2);
    }

    #[test]
    fn scope_duplicate_identifiers() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        // Same project specified twice via slug and human_key
        let result = apply_project_scope(
            &db,
            &["proj-alpha".to_string(), "/data/projects/alpha".to_string()],
        )
        .unwrap();
        assert_eq!(result.removed_count, 1);
        assert_eq!(
            result.projects.len(),
            1,
            "should de-duplicate by project ID"
        );
        assert_eq!(result.projects[0].slug, "proj-alpha");
    }

    #[test]
    fn scope_keeps_all_when_all_projects_matched() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let result =
            apply_project_scope(&db, &["proj-alpha".to_string(), "proj-beta".to_string()]).unwrap();
        assert_eq!(result.removed_count, 0);
        assert_eq!(result.remaining.messages, 3);
        assert_eq!(result.remaining.agents, 2);
        assert_eq!(result.remaining.inbox_stats, 2);
        assert_eq!(result.remaining.agent_links, 1);
        assert_eq!(result.remaining.project_sibling_suggestions, 1);
    }

    #[test]
    fn scope_by_slug_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let result = apply_project_scope(&db, &["PROJ-ALPHA".to_string()]).unwrap();
        assert_eq!(result.removed_count, 1);
        assert_eq!(result.projects[0].slug, "proj-alpha");
    }

    #[test]
    fn build_placeholders_empty() {
        let result = build_placeholders(0);
        assert_eq!(result, "");
    }

    #[test]
    fn build_placeholders_one() {
        let result = build_placeholders(1);
        assert_eq!(result, "?");
    }

    #[test]
    fn build_placeholders_many() {
        let result = build_placeholders(5);
        assert_eq!(result, "?,?,?,?,?");
    }

    #[test]
    fn scope_without_optional_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("no_links.sqlite3");
        let conn = Conn::open_file(db_path.display().to_string()).unwrap();

        // Create schema without agent_links and project_sibling_suggestions
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL, created_at TEXT DEFAULT '')",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT DEFAULT '', model TEXT DEFAULT '', task_description TEXT DEFAULT '', inception_ts TEXT DEFAULT '', last_active_ts TEXT DEFAULT '', attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, created_ts TEXT DEFAULT '', attachments TEXT DEFAULT '[]')",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, kind TEXT DEFAULT 'to', read_ts TEXT, ack_ts TEXT, PRIMARY KEY(message_id, agent_id))",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, agent_id INTEGER, path_pattern TEXT, exclusive INTEGER DEFAULT 1, reason TEXT DEFAULT '', created_ts TEXT DEFAULT '', expires_ts TEXT DEFAULT '', released_ts TEXT)",
        ).unwrap();

        conn.execute_raw("INSERT INTO projects VALUES (1, 'only', '/data/only', '2025-01-01')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (1, 1, 'Solo', '', '', '', '', '', 'auto', 'auto')",
        )
        .unwrap();
        drop(conn);

        let result = apply_project_scope(&db_path, &[]).unwrap();
        assert_eq!(result.remaining.projects, 1);
        assert_eq!(result.remaining.agent_links, 0);
        assert_eq!(result.remaining.inbox_stats, 0);
        assert_eq!(result.remaining.project_sibling_suggestions, 0);
    }

    #[test]
    fn project_scope_result_serialization_roundtrip() {
        let result = ProjectScopeResult {
            identifiers: vec!["proj-alpha".to_string()],
            projects: vec![ProjectRecord {
                id: 1,
                slug: "proj-alpha".to_string(),
                human_key: "/data/alpha".to_string(),
            }],
            removed_count: 1,
            remaining: RemainingCounts {
                projects: 1,
                agents: 2,
                messages: 5,
                recipients: 10,
                file_reservations: 3,
                inbox_stats: 1,
                agent_links: 0,
                project_sibling_suggestions: 0,
            },
        };

        let json = serde_json::to_string(&result).unwrap();
        let deserialized: ProjectScopeResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.removed_count, 1);
        assert_eq!(deserialized.projects.len(), 1);
        assert_eq!(deserialized.projects[0].slug, "proj-alpha");
        assert_eq!(deserialized.remaining.messages, 5);
    }

    #[test]
    fn scope_select_only_beta_project() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let result = apply_project_scope(&db, &["proj-beta".to_string()]).unwrap();
        assert_eq!(result.removed_count, 1);
        assert_eq!(result.projects.len(), 1);
        assert_eq!(result.projects[0].slug, "proj-beta");
        assert_eq!(result.remaining.projects, 1);
        assert_eq!(result.remaining.agents, 1);
        assert_eq!(result.remaining.messages, 1);
        assert_eq!(result.remaining.recipients, 1);
        assert_eq!(result.remaining.file_reservations, 0);
        assert_eq!(result.remaining.inbox_stats, 1);
    }

    #[test]
    fn scope_removes_release_markers_for_filtered_out_reservations() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("release_ledger.sqlite3");
        let conn = Conn::open_file(db_path.display().to_string()).unwrap();

        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL, created_at TEXT DEFAULT '')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT DEFAULT '', model TEXT DEFAULT '', task_description TEXT DEFAULT '', inception_ts TEXT DEFAULT '', last_active_ts TEXT DEFAULT '', attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, created_ts TEXT DEFAULT '', attachments TEXT DEFAULT '[]')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, kind TEXT DEFAULT 'to', read_ts TEXT, ack_ts TEXT, PRIMARY KEY(message_id, agent_id))",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, agent_id INTEGER, path_pattern TEXT, exclusive INTEGER DEFAULT 1, reason TEXT DEFAULT '', created_ts TEXT DEFAULT '', expires_ts TEXT DEFAULT '', released_ts TEXT)",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY, released_ts INTEGER NOT NULL)",
        )
        .unwrap();

        conn.execute_raw("INSERT INTO projects VALUES (1, 'proj-alpha', '/data/alpha', '')")
            .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (2, 'proj-beta', '/data/beta', '')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (1, 1, 'Alpha', '', '', '', '', '', 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (2, 2, 'Beta', '', '', '', '', '', 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO file_reservations VALUES (10, 1, 1, 'src/a.rs', 1, '', '', '', NULL)",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO file_reservations VALUES (20, 2, 2, 'src/b.rs', 1, '', '', '', NULL)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO file_reservation_releases VALUES (10, 111)")
            .unwrap();
        conn.execute_raw("INSERT INTO file_reservation_releases VALUES (20, 222)")
            .unwrap();
        drop(conn);

        let result = apply_project_scope(&db_path, &["proj-beta".to_string()]).unwrap();
        assert_eq!(result.remaining.file_reservations, 1);

        let conn = Conn::open_file(db_path.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT reservation_id FROM file_reservation_releases ORDER BY reservation_id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let reservation_id: i64 = rows[0].get_named("reservation_id").unwrap();
        assert_eq!(reservation_id, 20);
    }

    #[test]
    fn scope_empty_database_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("empty.sqlite3");
        let conn = Conn::open_file(db_path.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL)",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT DEFAULT '', model TEXT DEFAULT '', task_description TEXT DEFAULT '', inception_ts TEXT DEFAULT '', last_active_ts TEXT DEFAULT '', attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, created_ts TEXT DEFAULT '', attachments TEXT DEFAULT '[]')",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, kind TEXT DEFAULT 'to', read_ts TEXT, ack_ts TEXT, PRIMARY KEY(message_id, agent_id))",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, agent_id INTEGER, path_pattern TEXT, exclusive INTEGER DEFAULT 1, reason TEXT DEFAULT '', created_ts TEXT DEFAULT '', expires_ts TEXT DEFAULT '', released_ts TEXT)",
        ).unwrap();
        drop(conn);

        let result = apply_project_scope(&db_path, &["anything".to_string()]);
        assert!(matches!(result, Err(ShareError::ScopeNoProjects)));
    }

    #[test]
    fn scope_accepts_orphaned_project_placeholder_identifier() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();
        conn.execute_raw("DELETE FROM projects WHERE id = 1")
            .unwrap();
        drop(conn);

        let result = apply_project_scope(&db, &["[unknown-project-1]".to_string()]).unwrap();
        assert_eq!(result.removed_count, 1);
        assert_eq!(result.projects.len(), 1);
        assert_eq!(result.projects[0].id, 1);
        assert_eq!(result.projects[0].slug, "[unknown-project-1]");
        assert_eq!(result.remaining.projects, 0);
        assert_eq!(result.remaining.agents, 1);
        assert_eq!(result.remaining.messages, 2);
        assert_eq!(result.remaining.recipients, 2);
        assert_eq!(result.remaining.file_reservations, 1);
        assert_eq!(result.remaining.inbox_stats, 1);
    }

    #[test]
    fn scope_removes_inbox_stats_for_filtered_out_agents() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let result = apply_project_scope(&db, &["proj-alpha".to_string()]).unwrap();
        assert_eq!(result.remaining.inbox_stats, 1);

        let conn = Conn::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT agent_id, total_count FROM inbox_stats ORDER BY agent_id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let agent_id: i64 = rows[0].get_named("agent_id").unwrap();
        let total_count: i64 = rows[0].get_named("total_count").unwrap();
        assert_eq!(agent_id, 1);
        assert_eq!(total_count, 2);
    }

    #[test]
    fn scope_rebuilds_inbox_stats_for_kept_agents_after_message_trimming() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();
        conn.execute_raw(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (3, 1, 'cc')",
        )
        .unwrap();
        conn.execute_raw(
            "UPDATE inbox_stats \
             SET total_count = 3, unread_count = 3, ack_pending_count = 1, last_message_ts = 20 \
             WHERE agent_id = 1",
        )
        .unwrap();

        let result = apply_project_scope(&db, &["proj-alpha".to_string()]).unwrap();
        assert_eq!(result.remaining.inbox_stats, 1);

        let rows = conn
            .query_sync(
                "SELECT total_count, unread_count, ack_pending_count \
                 FROM inbox_stats WHERE agent_id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<i64>("total_count").unwrap(), 2);
        assert_eq!(rows[0].get_named::<i64>("unread_count").unwrap(), 2);
        assert_eq!(rows[0].get_named::<i64>("ack_pending_count").unwrap(), 0);
    }

    #[test]
    fn scope_removes_filtered_recipients_from_kept_messages_and_recipients_json() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();
        conn.execute_raw(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (1, 2, 'cc')",
        )
        .unwrap();
        conn.execute_raw(
            "UPDATE messages SET recipients_json = '{\"to\":[\"GreenCastle\"],\"cc\":[\"PurpleBear\"],\"bcc\":[]}' WHERE id = 1",
        )
        .unwrap();
        drop(conn);

        let result = apply_project_scope(&db, &["proj-alpha".to_string()]).unwrap();
        assert_eq!(result.remaining.recipients, 2);

        let conn = Conn::open_file(db.display().to_string()).unwrap();
        let recipient_rows = conn
            .query_sync(
                "SELECT agent_id, kind FROM message_recipients WHERE message_id = 1 ORDER BY agent_id",
                &[],
            )
            .unwrap();
        assert_eq!(recipient_rows.len(), 1);
        let kept_agent_id: i64 = recipient_rows[0].get_named("agent_id").unwrap();
        let kept_kind: String = recipient_rows[0].get_named("kind").unwrap();
        assert_eq!(kept_agent_id, 1);
        assert_eq!(kept_kind, "to");

        let message_rows = conn
            .query_sync("SELECT recipients_json FROM messages WHERE id = 1", &[])
            .unwrap();
        let recipients_json: String = message_rows[0].get_named("recipients_json").unwrap();
        let payload: serde_json::Value = serde_json::from_str(&recipients_json).unwrap();
        assert_eq!(payload, json!({"to":["GreenCastle"],"cc":[],"bcc":[]}));
    }

    #[test]
    fn scope_removes_filtered_product_links_and_unreferenced_products() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE products (
                id INTEGER PRIMARY KEY,
                product_uid TEXT NOT NULL,
                name TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT ''
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE product_project_links (
                id INTEGER PRIMARY KEY,
                product_id INTEGER NOT NULL,
                project_id INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT ''
            )",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO products VALUES (10, 'prod-alpha', 'Product Alpha', '')")
            .unwrap();
        conn.execute_raw("INSERT INTO products VALUES (20, 'prod-beta', 'Product Beta', '')")
            .unwrap();
        conn.execute_raw("INSERT INTO product_project_links VALUES (100, 10, 1, '')")
            .unwrap();
        conn.execute_raw("INSERT INTO product_project_links VALUES (200, 20, 2, '')")
            .unwrap();
        drop(conn);

        let result = apply_project_scope(&db, &["proj-alpha".to_string()]).unwrap();
        assert_eq!(result.removed_count, 1);

        let conn = Conn::open_file(db.display().to_string()).unwrap();
        let remaining_links = conn
            .query_sync(
                "SELECT product_id, project_id FROM product_project_links ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(remaining_links.len(), 1);
        let product_id: i64 = remaining_links[0].get_named("product_id").unwrap();
        let project_id: i64 = remaining_links[0].get_named("project_id").unwrap();
        assert_eq!(product_id, 10);
        assert_eq!(project_id, 1);

        let remaining_products = conn
            .query_sync("SELECT id FROM products ORDER BY id", &[])
            .unwrap();
        assert_eq!(remaining_products.len(), 1);
        let kept_product_id: i64 = remaining_products[0].get_named("id").unwrap();
        assert_eq!(kept_product_id, 10);
    }

    #[test]
    fn scope_refreshes_derived_export_artifacts_after_finalized_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();
        conn.execute_raw(
            "UPDATE messages \
             SET attachments = '[{\"type\":\"image\",\"media_type\":\"image/png\",\"path\":\"beta.png\",\"bytes\":42}]' \
             WHERE id = 3",
        )
        .unwrap();
        drop(conn);

        crate::build_materialized_views(&db, false).unwrap();
        crate::create_performance_indexes(&db).unwrap();

        let conn = Conn::open_file(db.display().to_string()).unwrap();
        let overview_before = conn
            .query_sync("SELECT id FROM message_overview_mv ORDER BY id", &[])
            .unwrap();
        assert_eq!(overview_before.len(), 3);
        let attachments_before = conn
            .query_sync(
                "SELECT message_id FROM attachments_by_message_mv ORDER BY message_id",
                &[],
            )
            .unwrap();
        assert_eq!(attachments_before.len(), 1);
        let attached_message_id: i64 = attachments_before[0].get_named("message_id").unwrap();
        assert_eq!(attached_message_id, 3);
        drop(conn);

        let result = apply_project_scope(&db, &["proj-alpha".to_string()]).unwrap();
        assert_eq!(result.remaining.messages, 2);

        let conn = Conn::open_file(db.display().to_string()).unwrap();
        let overview_after = conn
            .query_sync("SELECT id FROM message_overview_mv ORDER BY id", &[])
            .unwrap();
        let overview_ids: Vec<i64> = overview_after
            .iter()
            .map(|row| row.get_named("id").unwrap())
            .collect();
        assert_eq!(overview_ids, vec![1, 2]);

        let attachments_after = conn
            .query_sync(
                "SELECT message_id FROM attachments_by_message_mv ORDER BY message_id",
                &[],
            )
            .unwrap();
        assert!(attachments_after.is_empty());
    }

    /// Conformance test: scope against the fixture `needs_scrub.sqlite3` and
    /// compare with `expected_scoped.json` produced by the Python reference.
    #[test]
    fn conformance_scope_against_fixture() {
        let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mcp-agent-mail-conformance/tests/conformance/fixtures/share");

        let source = fixture_dir.join("needs_scrub.sqlite3");
        if !source.exists() {
            eprintln!(
                "Skipping conformance test: fixture not found at {}",
                source.display()
            );
            return;
        }

        let expected_path = fixture_dir.join("expected_scoped.json");
        let expected_text = std::fs::read_to_string(&expected_path).unwrap();
        let expected: serde_json::Value = serde_json::from_str(&expected_text).unwrap();

        // Create a snapshot copy so we don't modify the fixture
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("scoped.sqlite3");
        crate::create_sqlite_snapshot(&source, &snapshot, false).unwrap();

        // Apply scoping to just proj-alpha
        let result = apply_project_scope(&snapshot, &["proj-alpha".to_string()]).unwrap();

        // Compare
        let actual = serde_json::to_value(&result).unwrap();

        assert_eq!(
            actual["identifiers"], expected["identifiers"],
            "identifiers mismatch"
        );
        assert_eq!(
            actual["removed_count"], expected["removed_count"],
            "removed_count mismatch"
        );

        // Compare remaining counts
        let ar = &actual["remaining"];
        let er = &expected["remaining"];
        assert_eq!(ar["projects"], er["projects"], "remaining.projects");
        assert_eq!(ar["agents"], er["agents"], "remaining.agents");
        assert_eq!(ar["messages"], er["messages"], "remaining.messages");
        assert_eq!(ar["recipients"], er["recipients"], "remaining.recipients");
        assert_eq!(
            ar["file_reservations"], er["file_reservations"],
            "remaining.file_reservations"
        );
        if er.get("inbox_stats").is_some() {
            assert_eq!(
                ar["inbox_stats"], er["inbox_stats"],
                "remaining.inbox_stats"
            );
        }
        assert_eq!(
            ar["agent_links"], er["agent_links"],
            "remaining.agent_links"
        );
        assert_eq!(
            ar["project_sibling_suggestions"], er["project_sibling_suggestions"],
            "remaining.project_sibling_suggestions"
        );

        // Compare matched projects
        assert_eq!(actual["projects"].as_array().unwrap().len(), 1);
        let proj = &actual["projects"][0];
        let exp_proj = &expected["projects"][0];
        assert_eq!(proj["slug"], exp_proj["slug"]);
        assert_eq!(proj["human_key"], exp_proj["human_key"]);
        assert_eq!(proj["id"], exp_proj["id"]);
    }
}
