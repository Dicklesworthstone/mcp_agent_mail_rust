//! Bounded cold overview collection without SQL joins (GH#274).
//!
//! Keyset pages bound the Rust row buffers independently of mailbox size.
//! Correlate pending recipient pages with indexed message-ID lookups, and
//! consult the release ledger only for the current active-reservation page.
//! The returned project list necessarily remains proportional to projects.

use std::collections::{HashMap, HashSet};

use super::{OutputFormat, OverviewProject, RobotEnvelope, format_output};
use crate::CliError;
use mcp_agent_mail_db::DbConn;
use serde::Serialize;
use sqlmodel_core::{Row, Value};

const ACK_OVERDUE_THRESHOLD_US: i64 = 30 * 60 * 1_000_000;
// Below the conservative SQLite limit of 999 bindings, including the time
// parameter in a message lookup. Never build an unbounded IN parameter list.
const OVERVIEW_PAGE_ROWS: usize = 256;

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

// Used only by the frozen pre-batching reference implementation in tests.
#[cfg(test)]
fn release_ledger_index(conn: &DbConn, present: bool) -> Result<HashSet<i64>, CliError> {
    if !present {
        return Ok(HashSet::new());
    }
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
const AGENTS_SQL: &str = "SELECT id, project_id FROM agents";
const MESSAGE_INVENTORY_SQL: &str = "SELECT id, project_id FROM messages";
const MESSAGES_SQL: &str = "SELECT id, project_id,
    CASE WHEN importance IN ('urgent', 'high') THEN 1 ELSE 0 END AS urgent,
    CASE WHEN ack_required = 1 AND created_ts < ? THEN 1 ELSE 0 END AS overdue
    FROM messages";
// The mailbox schema defines message_recipients as a rowid table.
// A recipient has no single-column public ID. Use its hidden rowid only as a
// cursor INSIDE this read snapshot, never as a persisted identity. Row order
// need not agree with message_id/agent_id and may contain negative keys/gaps.
const RECIPIENTS_SQL: &str = "SELECT _rowid_ AS id, message_id,
    CASE WHEN read_ts IS NULL THEN 1 ELSE 0 END AS unread,
    CASE WHEN ack_ts IS NULL THEN 1 ELSE 0 END AS unacked
    FROM message_recipients";
const RECIPIENT_FILTER: &str = "read_ts IS NULL OR ack_ts IS NULL";
const RESERVATIONS_SQL: &str = "SELECT fr.id, fr.project_id, fr.expires_ts FROM file_reservations fr";
const RELEASE_LOOKUP_SQL: &str = "SELECT reservation_id FROM file_reservation_releases";

#[derive(Clone, Copy)]
struct Message {
    project_id: i64,
    urgent: bool,
    overdue: bool,
}

/// Actual data statements and returned rows, excluding schema/savepoint probes.
/// `rows` covers inventory/recipient/reservation scans; indexed message and
/// release lookups are counted separately. Peaks measure Rust result buffers,
/// not the embedded engine's internal memory or VM work.
#[derive(Debug, Default, PartialEq, Eq)]
struct ScanWork {
    queries: usize,
    rows: [usize; 5],
    message_lookup_rows: usize,
    release_lookup_rows: usize,
    peak_query_rows: usize,
    peak_message_keys: usize,
    peak_release_keys: usize,
}

fn integer(row: &Row, column: &str) -> Result<i64, CliError> {
    row.get_by_name(column)
        .and_then(Value::as_i64)
        .ok_or_else(|| CliError::Other(format!("overview {column} returned a non-integer")))
}

fn bounded_query(
    conn: &DbConn,
    sql: &str,
    params: &[Value],
    work: &mut ScanWork,
) -> Result<Vec<Row>, CliError> {
    let rows = conn.query_sync(sql, params)
        .map_err(|error| CliError::Other(format!("overview query failed: {error}")))?;
    work.queries += 1;
    work.peak_query_rows = work.peak_query_rows.max(rows.len());
    if rows.len() > OVERVIEW_PAGE_ROWS {
        return Err(CliError::Other("overview query exceeded its row budget".to_string()));
    }
    Ok(rows)
}

struct Scan<'a> {
    select: &'a str,
    predicate: &'a str,
    key: &'a str,
    params: &'a [Value],
    slot: usize,
}

fn scan_pages<F>(
    conn: &DbConn,
    work: &mut ScanWork,
    scan: Scan<'_>,
    mut visit: F,
) -> Result<(), CliError>
where
    F: FnMut(&[Row], &mut ScanWork) -> Result<(), CliError>,
{
    let mut after = None;
    loop {
        let mut params = scan.params.to_vec();
        let continuation = if let Some(id) = after {
            params.push(Value::BigInt(id));
            format!(" AND {} > ?", scan.key)
        } else {
            String::new()
        };
        let sql = format!(
            "{} WHERE ({}){continuation} ORDER BY {} LIMIT {OVERVIEW_PAGE_ROWS}",
            scan.select, scan.predicate, scan.key,
        );
        let rows = bounded_query(conn, &sql, &params, work)?;
        work.rows[scan.slot] += rows.len();
        // Reject an invalid/non-progressing cursor instead of looping forever
        // or silently counting duplicate rows. None includes i64::MIN on the
        // first page; no arithmetic is needed even when the last key is MAX.
        for row in &rows {
            let id = integer(row, "id")?;
            if after.is_some_and(|previous| id <= previous) {
                return Err(CliError::Other("overview cursor did not advance".to_string()));
            }
            after = Some(id);
        }
        if !rows.is_empty() {
            visit(&rows, work)?;
        }
        if rows.len() < OVERVIEW_PAGE_ROWS {
            return Ok(());
        }
    }
}

/// Walk the expiry index, not the entire reservation history in ID order.
/// Separate the equal-expiry tail from the next expiry range so a large group
/// sharing one expiry can seek by ID rather than repeatedly skipping its prefix.
fn scan_reservation_pages<F>(
    conn: &DbConn,
    work: &mut ScanWork,
    predicate: &str,
    now_us: i64,
    mut visit: F,
) -> Result<(), CliError>
where
    F: FnMut(&[Row], &mut ScanWork) -> Result<(), CliError>,
{
    let mut expiry = now_us;
    let mut last_id = None;
    loop {
        let (condition, order, params) = if let Some(id) = last_id {
            ("fr.expires_ts = ? AND fr.id > ?", "fr.id",
                vec![Value::BigInt(expiry), Value::BigInt(id)])
        } else {
            ("fr.expires_ts > ?", "fr.expires_ts, fr.id", vec![Value::BigInt(expiry)])
        };
        let sql = format!("{RESERVATIONS_SQL} WHERE ({predicate}) AND {condition} \
            ORDER BY {order} LIMIT {OVERVIEW_PAGE_ROWS}");
        let rows = bounded_query(conn, &sql, &params, work)?;
        work.rows[4] += rows.len();
        let mut previous = last_id.map(|id| (expiry, id));
        for row in &rows {
            let next = (integer(row, "expires_ts")?, integer(row, "id")?);
            let valid_expiry = if last_id.is_some() { next.0 == expiry } else { next.0 > expiry };
            if !valid_expiry || previous.is_some_and(|value| next <= value) {
                return Err(CliError::Other("overview reservation cursor did not advance".to_string()));
            }
            previous = Some(next);
        }
        if !rows.is_empty() {
            visit(&rows, work)?;
        }
        if rows.len() < OVERVIEW_PAGE_ROWS {
            if last_id.is_none() {
                return Ok(());
            }
            // The equal-expiry group is exhausted; seek the next group.
            last_id = None;
        } else if let Some((next_expiry, next_id)) = previous {
            expiry = next_expiry;
            last_id = Some(next_id);
        }
    }
}

fn lookup_ids(
    conn: &DbConn,
    select: &str,
    key: &str,
    leading_params: &[Value],
    ids: &[i64],
    work: &mut ScanWork,
) -> Result<Vec<Row>, CliError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() > OVERVIEW_PAGE_ROWS {
        return Err(CliError::Other("overview lookup exceeded its key budget".to_string()));
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!("{select} WHERE {key} IN ({placeholders}) LIMIT {OVERVIEW_PAGE_ROWS}");
    let mut params = leading_params.to_vec();
    params.extend(ids.iter().copied().map(Value::BigInt));
    bounded_query(conn, &sql, &params, work)
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
    // One snapshot covers EVERY page and lookup, not one snapshot per batch.
    // This nests inside a caller's transaction without committing it.
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
    scan_pages(conn, &mut work, Scan {
        select: PROJECTS_SQL, predicate: "1 = 1", key: "id", params: &[], slot: 0,
    }, |rows, _| {
        for row in rows {
            let id = integer(row, "id")?;
            let slug = row.get_named::<String>("slug")
                .map_err(|error| CliError::Other(format!("overview project slug decode failed: {error}")))?;
            project(&mut projects, id).slug = slug;
        }
        Ok(())
    })?;

    // Inventory must still include agent-only/message-only orphan projects,
    // even if they have no pending recipients. No message map is kept here.
    for (select, slot) in [(AGENTS_SQL, 1), (MESSAGE_INVENTORY_SQL, 2)] {
        scan_pages(conn, &mut work, Scan {
            select, predicate: "1 = 1", key: "id", params: &[], slot,
        }, |rows, _| {
            for row in rows {
                project(&mut projects, integer(row, "project_id")?);
            }
            Ok(())
        })?;
    }

    scan_pages(conn, &mut work, Scan {
        select: RECIPIENTS_SQL, predicate: RECIPIENT_FILTER,
        key: "_rowid_", params: &[], slot: 3,
    }, |rows, work| {
        let mut ids: Vec<_> = rows.iter().filter_map(|row| {
            row.get_by_name("message_id").and_then(Value::as_i64)
        }).collect();
        ids.sort_unstable();
        ids.dedup();
        let message_rows = lookup_ids(conn, MESSAGES_SQL, "id",
            &[Value::BigInt(micros_ago(now_us, ACK_OVERDUE_THRESHOLD_US))], &ids, work)?;
        work.message_lookup_rows += message_rows.len();
        let mut messages = HashMap::with_capacity(message_rows.len());
        for row in message_rows {
            messages.insert(integer(&row, "id")?, Message {
                project_id: integer(&row, "project_id")?,
                urgent: integer(&row, "urgent")? != 0,
                overdue: integer(&row, "overdue")? != 0,
            });
        }
        work.peak_message_keys = work.peak_message_keys.max(messages.len());
        for row in rows {
            // Match the original INNER JOIN: dangling recipients contribute
            // nothing. Read-but-unacknowledged mail still counts as overdue.
            let Some(message) = row.get_by_name("message_id")
                .and_then(Value::as_i64).and_then(|id| messages.get(&id)) else {
                continue;
            };
            let counts = project(&mut projects, message.project_id);
            if integer(row, "unread")? != 0 {
                counts.unread += 1;
                counts.urgent += usize::from(message.urgent);
            }
            if message.overdue && integer(row, "unacked")? != 0 {
                counts.ack_overdue += 1;
            }
        }
        Ok(())
    })?;

    let has_ledger = has_file_reservation_release_ledger(conn)?;
    let legacy = has_file_reservations_released_ts_column(conn)?;
    let predicate = active_reservation_candidate_sql(legacy, "fr");
    scan_reservation_pages(conn, &mut work, &predicate, now_us, |rows, work| {
        let ids: Vec<_> = rows.iter().map(|row| integer(row, "id"))
            .collect::<Result<_, _>>()?;
        let mut released = HashSet::new();
        if has_ledger {
            let releases = lookup_ids(conn, RELEASE_LOOKUP_SQL, "reservation_id", &[], &ids, work)?;
            work.release_lookup_rows += releases.len();
            for row in releases {
                released.insert(integer(&row, "reservation_id")?);
            }
        }
        work.peak_release_keys = work.peak_release_keys.max(released.len());
        for row in rows {
            // Membership releases even with a NULL/zero ledger timestamp.
            if !released.contains(&integer(row, "id")?) {
                project(&mut projects, integer(row, "project_id")?).reservations += 1;
            }
        }
        Ok(())
    })?;

    let mut projects: Vec<_> = projects.into_iter().collect();
    projects.sort_by(|(left_id, left), (right_id, right)| {
        left.slug.cmp(&right.slug).then(left_id.cmp(right_id))
    });
    Ok((projects.into_iter().map(|(_, row)| row).collect(), work))
}

#[cfg(test)]
mod tests;
