//! Step 2: Project scoping — delete rows for non-selected projects.
//!
//! Given a snapshot database and a list of project identifiers (slugs or
//! human_keys), removes all data belonging to non-selected projects.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sqlmodel_core::Value;

use crate::ShareError;

/// Connection type for offline snapshot manipulation (C-backed SQLite).
type Conn = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection;

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
/// - [`ShareError::ScopeNoProjects`] if the database has no projects.
/// - [`ShareError::ScopeIdentifierNotFound`] if any identifier doesn't match.
/// - [`ShareError::Sqlite`] on any SQLite error.
pub fn apply_project_scope(
    snapshot_path: &Path,
    identifiers: &[String],
) -> Result<ProjectScopeResult, ShareError> {
    let path_str = snapshot_path.display().to_string();
    // Use SqliteConnection (C-backed) for offline snapshot manipulation.
    // FrankenConnection lacks sqlite_master support needed by table_exists().
    let conn = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection::open_file(&path_str).map_err(
        |e| ShareError::Sqlite {
            message: format!("cannot open snapshot {path_str}: {e}"),
        },
    )?;

    // Enable foreign keys
    conn.execute_raw("PRAGMA foreign_keys = ON")
        .map_err(|e| ShareError::Sqlite {
            message: format!("PRAGMA foreign_keys failed: {e}"),
        })?;

    // Load all projects
    let project_rows = conn
        .query_sync("SELECT id, slug, human_key FROM projects", &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("SELECT projects failed: {e}"),
        })?;

    if project_rows.is_empty() {
        return Err(ShareError::ScopeNoProjects);
    }

    let all_projects: Vec<ProjectRecord> = project_rows
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
            "SELECT id FROM messages WHERE project_id NOT IN ({p})",
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

        // 7. Delete agents
        exec(
            &conn,
            &format!(
                "DELETE FROM agents WHERE project_id NOT IN ({p})",
                p = placeholders
            ),
            &id_values,
        )?;

        // 8. Delete projects
        exec(
            &conn,
            &format!(
                "DELETE FROM projects WHERE id NOT IN ({p})",
                p = placeholders
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
            Ok(out)
        }
        Err(err) => {
            let _ = conn.execute_sync("ROLLBACK", &[]);
            Err(err)
        }
    }
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
fn table_exists(conn: &Conn, name: &str) -> Result<bool, ShareError> {
    let rows = conn
        .query_sync(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
            &[Value::Text(name.to_string())],
        )
        .map_err(|e| ShareError::Sqlite {
            message: format!("table_exists check failed: {e}"),
        })?;
    Ok(!rows.is_empty())
}

/// Execute a statement with parameters, mapping errors to [`ShareError`].
fn exec(conn: &Conn, sql: &str, params: &[Value]) -> Result<u64, ShareError> {
    conn.execute_sync(sql, params)
        .map_err(|e| ShareError::Sqlite {
            message: format!("SQL exec failed: {e}"),
        })
}

/// Count remaining rows in all relevant tables.
fn count_remaining(conn: &Conn) -> Result<RemainingCounts, ShareError> {
    Ok(RemainingCounts {
        projects: count_table(conn, "projects")?,
        agents: count_table(conn, "agents")?,
        messages: count_table(conn, "messages")?,
        recipients: count_table(conn, "message_recipients")?,
        file_reservations: count_table(conn, "file_reservations")?,
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
        let conn = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection::open_file(
            db_path.display().to_string(),
        )
        .unwrap();
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
        conn.execute_raw("INSERT INTO agent_links (a_project_id, a_agent_id, b_project_id, b_agent_id) VALUES (1, 1, 2, 2)")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO project_sibling_suggestions (project_a_id, project_b_id) VALUES (1, 2)",
        )
        .unwrap();

        db_path
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
        let conn = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection::open_file(
            db.display().to_string(),
        )
        .unwrap();
        let result = count_table(&conn, "unknown_table");
        assert!(matches!(result, Err(ShareError::Sqlite { .. })));
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
