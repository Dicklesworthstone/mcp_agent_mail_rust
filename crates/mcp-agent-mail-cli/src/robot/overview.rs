//! Cold overview collection without SQL joins or per-project queries (GH#274).
//!
//! Keep the projections narrow: message bodies and reservation paths are not
//! needed. Each table is read once; message/recipient correlation is a hash
//! lookup, independent of the embedded SQL engine's join strategy.

use std::collections::{HashMap, HashSet};

use super::{OutputFormat, OverviewProject, RobotEnvelope, format_output};
use crate::CliError;
use mcp_agent_mail_db::DbConn;
use serde::Serialize;
use sqlmodel_core::{Row, Value};

// Same 30-minute, strictly-older-than boundary as inbox/status.
const ACK_OVERDUE_THRESHOLD_US: i64 = 30 * 60 * 1_000_000;

fn micros_ago(now: i64, delta: i64) -> i64 {
    now.saturating_sub(delta)
}

fn has_column(conn: &DbConn, sql: &str, column: Option<&str>) -> Result<bool, CliError> {
    let rows = conn.query_sync(sql, &[])
        .map_err(|error| CliError::Other(format!("overview schema probe failed: {error}")))?;
    Ok(match column {
        None => !rows.is_empty(),
        Some(column) => rows.iter().any(|row| {
            row.get_named::<String>("name").ok().as_deref() == Some(column)
        }),
    })
}

fn has_file_reservation_release_ledger(conn: &DbConn) -> Result<bool, CliError> {
    has_column(conn, "PRAGMA table_info(file_reservation_releases)", None)
}

fn has_file_reservations_released_ts_column(conn: &DbConn) -> Result<bool, CliError> {
    has_column(conn, "PRAGMA table_info(file_reservations)", Some("released_ts"))
}

fn release_ledger_index(conn: &DbConn, present: bool) -> Result<HashSet<i64>, CliError> {
    if !present {
        return Ok(HashSet::new());
    }
    // Existence releases a reservation even when released_ts is NULL or zero.
    conn.query_sync("SELECT reservation_id FROM file_reservation_releases", &[])
        .map_err(|error| CliError::Other(format!("overview release ledger query failed: {error}")))?
        .iter()
        .map(|row| integer(row, "reservation_id"))
        .collect()
}

fn active_reservation_candidate_sql(legacy: bool, alias: &str) -> String {
    if legacy {
        mcp_agent_mail_db::queries::active_reservation_candidate_predicate_for(alias)
    } else {
        "1 = 1".to_string()
    }
}

pub(super) fn render(
    projects: &[OverviewProject], counts: bool, format: OutputFormat,
) -> Result<String, CliError> {
    #[derive(Serialize)]
    struct Full<'a> {
        project_count: usize,
        projects: &'a [OverviewProject],
    }
    #[derive(Serialize)]
    struct Counts {
        project_count: usize,
        unread: usize,
        urgent: usize,
        ack_overdue: usize,
    }
    if counts {
        let mut totals = Counts { project_count: projects.len(), unread: 0, urgent: 0, ack_overdue: 0 };
        for project in projects {
            totals.unread = totals.unread.saturating_add(project.unread);
            totals.urgent = totals.urgent.saturating_add(project.urgent);
            totals.ack_overdue = totals.ack_overdue.saturating_add(project.ack_overdue);
        }
        format_output(&RobotEnvelope::new("robot overview", format, totals), format)
    } else {
        format_output(&RobotEnvelope::new("robot overview", format,
            Full { project_count: projects.len(), projects }), format)
    }
}

const PROJECTS_SQL: &str = "SELECT id, slug FROM projects";
const AGENTS_SQL: &str = "SELECT project_id FROM agents";
const MESSAGES_SQL: &str = "SELECT id, project_id,
    CASE WHEN importance IN ('urgent', 'high') THEN 1 ELSE 0 END AS urgent,
    CASE WHEN ack_required = 1 AND created_ts < ? THEN 1 ELSE 0 END AS overdue
    FROM messages";
const RECIPIENTS_SQL: &str = "SELECT message_id,
    CASE WHEN read_ts IS NULL THEN 1 ELSE 0 END AS unread,
    CASE WHEN ack_ts IS NULL THEN 1 ELSE 0 END AS unacked
    FROM message_recipients WHERE read_ts IS NULL OR ack_ts IS NULL";

#[derive(Clone, Copy)]
struct Message {
    project_id: i64,
    urgent: bool,
    overdue: bool,
}

/// Work performed by the five data scans, excluding schema probes and the
/// release-ledger scan. Counts returned rows, not SQLite VM steps. Tests avoid timing budgets.
#[derive(Debug, Default, PartialEq, Eq)]
struct ScanWork {
    queries: usize,
    rows: [usize; 5],
}

fn integer(row: &Row, column: &str) -> Result<i64, CliError> {
    row.get_by_name(column)
        .and_then(Value::as_i64)
        .ok_or_else(|| CliError::Other(format!("overview {column} returned a non-integer")))
}

fn query(
    conn: &DbConn,
    sql: &str,
    params: &[Value],
    work: &mut ScanWork,
    scan: usize,
) -> Result<Vec<Row>, CliError> {
    let rows = conn
        .query_sync(sql, params)
        .map_err(|error| CliError::Other(format!("overview scan {scan} failed: {error}")))?;
    work.queries += 1;
    work.rows[scan] += rows.len();
    Ok(rows)
}

fn project(projects: &mut HashMap<i64, OverviewProject>, id: i64) -> &mut OverviewProject {
    projects.entry(id).or_insert_with(|| OverviewProject {
        slug: format!("[unknown-project-{id}]"),
        unread: 0,
        urgent: 0,
        ack_overdue: 0,
        reservations: 0,
    })
}

pub(super) fn build(conn: &DbConn) -> Result<Vec<OverviewProject>, CliError> {
    build_at(conn, mcp_agent_mail_db::now_micros()).map(|(projects, _)| projects)
}

fn build_at(conn: &DbConn, now_us: i64) -> Result<(Vec<OverviewProject>, ScanWork), CliError> {
    // A savepoint works on query-only connections and nests inside a caller's
    // transaction. It does not commit, roll back or acquire a write lock for it.
    conn.execute_sync("SAVEPOINT robot_overview_read", &[])
        .map_err(|error| CliError::Other(format!("overview snapshot begin failed: {error}")))?;
    let result = collect_at(conn, now_us);
    let release = conn.execute_sync("RELEASE robot_overview_read", &[]);
    match (result, release) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(CliError::Other(format!("overview snapshot end failed: {error}"))),
        (Ok(result), Ok(_)) => Ok(result),
    }
}

fn collect_at(conn: &DbConn, now_us: i64) -> Result<(Vec<OverviewProject>, ScanWork), CliError> {
    let mut work = ScanWork::default();
    let mut projects = HashMap::new();
    for row in query(conn, PROJECTS_SQL, &[], &mut work, 0)? {
        let id = integer(&row, "id")?;
        let slug = row
            .get_named::<String>("slug")
            .map_err(|error| CliError::Other(format!("overview project slug decode failed: {error}")))?;
        project(&mut projects, id).slug = slug;
    }

    // Agents and messages also make orphan projects visible, even when there
    // are no recipients. Deduplication is by project ID, not display slug.
    for row in query(conn, AGENTS_SQL, &[], &mut work, 1)? {
        project(&mut projects, integer(&row, "project_id")?);
    }
    let message_rows = query(
        conn,
        MESSAGES_SQL,
        &[Value::BigInt(micros_ago(now_us, ACK_OVERDUE_THRESHOLD_US))],
        &mut work,
        2,
    )?;
    let mut messages = HashMap::with_capacity(message_rows.len());
    for row in message_rows {
        let id = integer(&row, "id")?;
        let project_id = integer(&row, "project_id")?;
        project(&mut projects, project_id);
        messages.insert(
            id,
            Message {
                project_id,
                urgent: integer(&row, "urgent")? != 0,
                overdue: integer(&row, "overdue")? != 0,
            },
        );
    }

    for row in query(conn, RECIPIENTS_SQL, &[], &mut work, 3)? {
        // Match the old INNER JOIN: dangling recipients contribute nothing.
        let Some(message) = row
            .get_by_name("message_id")
            .and_then(Value::as_i64)
            .and_then(|id| messages.get(&id))
        else {
            continue;
        };
        let counts = project(&mut projects, message.project_id);
        if integer(&row, "unread")? != 0 {
            counts.unread += 1;
            counts.urgent += usize::from(message.urgent);
        }
        // Read-but-unacknowledged messages still count as ack-overdue.
        if message.overdue && integer(&row, "unacked")? != 0 {
            counts.ack_overdue += 1;
        }
    }
    drop(messages);

    // Preserve the already-fixed release-ledger semantics. Membership, not the
    // ledger timestamp's nullability, determines whether a release exists.
    let has_ledger = has_file_reservation_release_ledger(conn)?;
    let legacy = has_file_reservations_released_ts_column(conn)?;
    let ledger = release_ledger_index(conn, has_ledger)?;
    let predicate = active_reservation_candidate_sql(legacy, "fr");
    let reservation_sql = format!(
        "SELECT fr.id, fr.project_id FROM file_reservations fr
         WHERE ({predicate}) AND fr.expires_ts > ?"
    );
    for row in query(
        conn,
        &reservation_sql,
        &[Value::BigInt(now_us)],
        &mut work,
        4,
    )? {
        if !ledger.contains(&integer(&row, "id")?) {
            project(&mut projects, integer(&row, "project_id")?).reservations += 1;
        }
    }

    let mut projects: Vec<_> = projects.into_iter().collect();
    projects.sort_by(|(left_id, left), (right_id, right)| {
        left.slug.cmp(&right.slug).then(left_id.cmp(right_id))
    });
    Ok((projects.into_iter().map(|(_, row)| row).collect(), work))
}

#[cfg(test)]
mod tests;
