//! Bounded cold overview collection without SQL joins (GH#274).
//!
//! Keyset pages bound the Rust row buffers independently of mailbox size.
//! Correlate pending recipient pages with indexed message-ID lookups, and
//! consult the release ledger only for the current active-reservation page.
//! Inventory seeks past already-discovered project groups through a suitable
//! index, rather than returning every historical message and agent row.
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
const PROJECT_INVENTORY_SQL: &str = "SELECT project_id FROM";
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
/// Inventory slots count returned keys, not the history skipped by index seeks.
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

fn quoted_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A project-leading, non-partial index is necessary for cheap group seeks.
/// Do not depend on a particular migration's index name: archive/read-only
/// fallback schemas may lack it. Missing or inconclusive index metadata uses
/// the original ID scan; an actual database read failure still propagates.
fn project_inventory_index(conn: &DbConn, table: &str) -> Result<Option<String>, CliError> {
    let indexes = conn.query_sync(
        &format!("PRAGMA index_list({})", quoted_identifier(table)), &[],
    ).map_err(|error| CliError::Other(format!("overview index inventory failed: {error}")))?;
    for index in indexes {
        if index.get_by_name("partial").and_then(Value::as_i64) != Some(0) {
            continue;
        }
        let Ok(name) = index.get_named::<String>("name") else { continue };
        let columns = conn.query_sync(
            &format!("PRAGMA index_info({})", quoted_identifier(&name)), &[],
        ).map_err(|error| CliError::Other(format!("overview index columns failed: {error}")))?;
        if columns.iter().any(|column| {
            column.get_by_name("seqno").and_then(Value::as_i64) == Some(0)
                && column.get_named::<String>("name").ok().as_deref() == Some("project_id")
        }) {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// Inventory needs presence, not multiplicity. A page may end partway through
/// a project's history: the next project_id > cursor seek skips that tail.
/// Every project appears in at most one page; every nonempty page discovers a
/// new project. With an index, at most min(history rows, PAGE * projects) keys
/// and no more data statements than the old ID scan are needed. A dense set of
/// one-row projects still uses full pages instead of one query per project.
fn scan_project_inventory(
    conn: &DbConn,
    work: &mut ScanWork,
    table: &str,
    fallback_select: &str,
    slot: usize,
    projects: &mut HashMap<i64, OverviewProject>,
) -> Result<(), CliError> {
    let Some(index) = project_inventory_index(conn, table)? else {
        return scan_pages(conn, work, Scan {
            select: fallback_select, predicate: "1 = 1", key: "id", params: &[], slot,
        }, |rows, _| {
            for row in rows {
                project(projects, integer(row, "project_id")?);
            }
            Ok(())
        });
    };
    let mut after = None;
    loop {
        let (predicate, params) = match after {
            Some(id) => ("WHERE project_id > ?", vec![Value::BigInt(id)]),
            None => ("", Vec::new()),
        };
        // Explicitly use the verified index so the engine cannot silently
        // substitute repeated history scans/sorts. The schema is held stable
        // by build_at's existing read snapshot; a refused hint is an error.
        let sql = format!(
            "{PROJECT_INVENTORY_SQL} {} INDEXED BY {} {predicate} \
             ORDER BY project_id LIMIT {OVERVIEW_PAGE_ROWS}",
            quoted_identifier(table), quoted_identifier(&index),
        );
        let rows = bounded_query(conn, &sql, &params, work)?;
        work.rows[slot] += rows.len();
        let mut previous = after;
        for row in &rows {
            let id = integer(row, "project_id")?;
            // Equal keys within a page are intentional; replaying an earlier
            // page or receiving unsorted/malformed results is not.
            if after.is_some_and(|cursor| id <= cursor)
                || previous.is_some_and(|cursor| id < cursor)
            {
                return Err(CliError::Other("overview project cursor did not advance".to_string()));
            }
            project(projects, id);
            previous = Some(id);
        }
        if rows.len() < OVERVIEW_PAGE_ROWS {
            return Ok(());
        }
        after = previous;
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
    // even if they have no pending recipients. Skip repeated history keys
    // when an appropriate full index is available; never create an index in
    // this read-only command or suppress an orphan to avoid scanning it.
    for (table, select, slot) in [
        ("agents", AGENTS_SQL, 1), ("messages", MESSAGE_INVENTORY_SQL, 2),
    ] {
        scan_project_inventory(conn, &mut work, table, select, slot, &mut projects)?;
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

#[cfg(test)]
mod inventory_tests {
    use super::*;
    use std::time::Instant;

    fn execute(conn: &DbConn, sql: &str) {
        conn.execute_sync(sql, &[]).expect(sql);
    }

    fn fixture() -> (tempfile::TempDir, DbConn) {
        let dir = tempfile::tempdir().expect("inventory fixture directory");
        let conn = DbConn::open_file(dir.path().join("inventory.sqlite3").to_str().unwrap())
            .expect("native inventory database");
        for sql in [
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL)",
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL)",
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, importance TEXT, ack_required INTEGER, created_ts INTEGER)",
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, read_ts INTEGER, ack_ts INTEGER)",
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, expires_ts INTEGER)",
        ] {
            execute(&conn, sql);
        }
        (dir, conn)
    }

    fn seed(conn: &DbConn, groups: &[(i64, usize)]) {
        let mut values = Vec::new();
        let mut id = 0;
        // Deliberately interleave project IDs in rowid order. The optimization
        // must follow project keys, not assume monotonically allocated IDs.
        for ordinal in 0..groups.iter().map(|(_, count)| *count).max().unwrap_or(0) {
            for &(project, count) in groups {
                if ordinal < count {
                    id += 1;
                    values.push(format!("({id}, {project}, 'high', 1, 0)"));
                }
            }
        }
        execute(conn, "BEGIN");
        for chunk in values.chunks(OVERVIEW_PAGE_ROWS) {
            execute(conn, &format!("INSERT INTO messages VALUES {}", chunk.join(",")));
        }
        execute(conn, "COMMIT");
    }

    fn inventory(conn: &DbConn, indexed: bool) -> (Vec<i64>, ScanWork) {
        let mut projects = HashMap::new();
        let mut work = ScanWork::default();
        if indexed {
            scan_project_inventory(conn, &mut work, "messages", MESSAGE_INVENTORY_SQL, 2, &mut projects)
                .expect("project-key inventory");
        } else {
            scan_pages(conn, &mut work, Scan {
                select: MESSAGE_INVENTORY_SQL, predicate: "1 = 1", key: "id", params: &[], slot: 2,
            }, |rows, _| {
                for row in rows {
                    project(&mut projects, integer(row, "project_id")?);
                }
                Ok(())
            }).expect("previous ID inventory");
        }
        let mut ids: Vec<_> = projects.into_keys().collect();
        ids.sort_unstable();
        (ids, work)
    }

    #[test]
    fn history_growth_does_not_grow_indexed_inventory_results() {
        for size in [OVERVIEW_PAGE_ROWS + 1, OVERVIEW_PAGE_ROWS * 4 + 13] {
            let (_dir, conn) = fixture();
            execute(&conn, "CREATE INDEX by_project ON messages(project_id, created_ts)");
            seed(&conn, &[(77, size), (-8, size), (1, size)]);
            let (expected, old) = inventory(&conn, false);
            let (actual, new) = inventory(&conn, true);
            assert_eq!(actual, expected);
            assert_eq!(actual, [-8, 1, 77]);
            assert_eq!(new.rows[2], 3 * OVERVIEW_PAGE_ROWS);
            assert_eq!(new.queries, 4);
            assert_eq!(old.rows[2], 3 * size);
            assert!(new.queries <= old.queries);
            assert_eq!(new.peak_query_rows, OVERVIEW_PAGE_ROWS);
        }
    }

    #[test]
    fn one_row_projects_still_use_batches_not_one_query_per_project() {
        let (_dir, conn) = fixture();
        execute(&conn, "CREATE INDEX by_project ON messages(project_id)");
        let groups: Vec<_> = (0..OVERVIEW_PAGE_ROWS * 2 + 5)
            .map(|id| (i64::try_from(id).unwrap() - 500, 1)).collect();
        seed(&conn, &groups);
        let (expected, old) = inventory(&conn, false);
        let (actual, new) = inventory(&conn, true);
        assert_eq!(actual, expected);
        assert_eq!(new.rows[2], groups.len());
        assert_eq!(new.queries, old.queries);
        assert_eq!(new.queries, 3);
    }

    #[test]
    fn duplicate_page_tail_and_extreme_project_ids_preserve_every_project() {
        let (_dir, conn) = fixture();
        execute(&conn, "CREATE INDEX by_project ON messages(project_id)");
        seed(&conn, &[(i64::MAX, OVERVIEW_PAGE_ROWS + 1), (0, 2), (i64::MIN, OVERVIEW_PAGE_ROWS - 1)]);
        let (ids, work) = inventory(&conn, true);
        assert_eq!(ids, [i64::MIN, 0, i64::MAX]);
        assert_eq!(work.rows[2], OVERVIEW_PAGE_ROWS * 2);
        assert_eq!(work.queries, 3);
    }

    #[test]
    fn missing_partial_expression_and_nonleading_indexes_keep_the_id_scan() {
        for ddl in [
            None,
            Some("CREATE INDEX unsuitable ON messages(project_id) WHERE project_id = 1"),
            Some("CREATE INDEX unsuitable ON messages((project_id + 0))"),
            Some("CREATE INDEX unsuitable ON messages(created_ts, project_id)"),
        ] {
            let (_dir, conn) = fixture();
            if let Some(ddl) = ddl { execute(&conn, ddl); }
            seed(&conn, &[(1, OVERVIEW_PAGE_ROWS + 3), (99, 1)]);
            assert!(project_inventory_index(&conn, "messages").unwrap().is_none());
            let expected = inventory(&conn, false);
            let actual = inventory(&conn, true);
            assert_eq!(actual, expected, "unsuitable index: {ddl:?}");
            assert_eq!(actual.0, [1, 99]);
        }
    }

    #[test]
    fn quoted_descending_composite_index_is_usable() {
        let (_dir, conn) = fixture();
        execute(&conn, "CREATE INDEX \"project\"\"history\" ON messages(project_id DESC, created_ts)");
        seed(&conn, &[(9, OVERVIEW_PAGE_ROWS + 3), (-4, 1)]);
        assert_eq!(project_inventory_index(&conn, "messages").unwrap().as_deref(), Some("project\"history"));
        let (ids, work) = inventory(&conn, true);
        assert_eq!(ids, [-4, 9]);
        assert_eq!(work.rows[2], OVERVIEW_PAGE_ROWS);
    }

    #[test]
    fn empty_index_has_one_empty_page_and_no_synthetic_project() {
        let (_dir, conn) = fixture();
        execute(&conn, "CREATE INDEX by_project ON messages(project_id)");
        let (ids, work) = inventory(&conn, true);
        assert!(ids.is_empty());
        assert_eq!(work.queries, 1);
        assert_eq!(work.rows[2], 0);
    }

    #[test]
    fn indexed_live_build_keeps_orphans_counts_and_caller_transaction() {
        let (_dir, conn) = fixture();
        execute(&conn, "CREATE INDEX messages_by_project ON messages(project_id, created_ts)");
        execute(&conn, "CREATE INDEX agents_by_project ON agents(project_id)");
        execute(&conn, "INSERT INTO projects VALUES (1, 'alpha'), (2, 'empty')");
        execute(&conn, "INSERT INTO agents VALUES (1, 777)");
        seed(&conn, &[(1, OVERVIEW_PAGE_ROWS * 2), (888, 1)]);
        execute(&conn, "INSERT INTO message_recipients VALUES (1, 1, NULL, NULL), (1, 2, 0, NULL)");
        let now = ACK_OVERDUE_THRESHOLD_US * 10;
        let (before, work) = build_at(&conn, now).unwrap();
        let alpha = before.iter().find(|row| row.slug == "alpha").unwrap();
        assert_eq!((alpha.unread, alpha.urgent, alpha.ack_overdue), (1, 1, 2));
        assert_eq!(before.len(), 4);
        assert!(before.iter().any(|row| row.slug == "[unknown-project-777]"));
        assert!(before.iter().any(|row| row.slug == "[unknown-project-888]"));
        assert_eq!(work.rows[2], OVERVIEW_PAGE_ROWS + 1);
        execute(&conn, "BEGIN");
        execute(&conn, "UPDATE messages SET project_id = 999 WHERE project_id = 888");
        let inside = build_at(&conn, now).unwrap().0;
        assert!(inside.iter().any(|row| row.slug == "[unknown-project-999]"));
        assert!(!inside.iter().any(|row| row.slug == "[unknown-project-888]"));
        execute(&conn, "ROLLBACK");
        execute(&conn, "PRAGMA query_only = ON");
        let after = build_at(&conn, now).unwrap().0;
        assert_eq!(serde_json::to_value(before).unwrap(), serde_json::to_value(after).unwrap());
    }

    #[test]
    fn index_probe_read_errors_are_not_reported_as_empty_inventory() {
        let (_dir, conn) = fixture();
        let mut work = ScanWork::default();
        let mut projects = HashMap::new();
        assert!(scan_project_inventory(&conn, &mut work, "missing_table",
            "SELECT id, project_id FROM missing_table", 2, &mut projects).is_err());
        assert!(projects.is_empty());
    }

    #[test]
    #[ignore = "native DbConn inventory benchmark, not end-to-end CLI latency"]
    fn benchmark_project_inventory_against_id_scan() {
        let (_dir, conn) = fixture();
        execute(&conn, "CREATE INDEX messages_by_project ON messages(project_id, created_ts)");
        let per_project: usize = std::env::var("AM_OVERVIEW_INVENTORY_MESSAGES_PER_PROJECT")
            .map_or(2000, |value| value.parse().expect("positive inventory fixture size"));
        assert!(per_project > 0);
        let groups: Vec<_> = (1..=33).map(|id| (id, per_project)).collect();
        seed(&conn, &groups);
        let mut old_times = Vec::new();
        let mut new_times = Vec::new();
        let expected = inventory(&conn, false).0;
        for iteration in 0..6 {
            for indexed in [iteration % 2 == 0, iteration % 2 != 0] {
                let start = Instant::now();
                let (ids, work) = inventory(&conn, indexed);
                let elapsed = start.elapsed();
                assert_eq!(ids, expected);
                eprintln!("indexed={indexed} elapsed={elapsed:?} work={work:?}");
                if indexed { new_times.push(elapsed); } else { old_times.push(elapsed); }
            }
        }
        old_times.sort_unstable();
        new_times.sort_unstable();
        eprintln!("33-project native inventory only; rows_per_project={per_project}; old_median={:?}; new_median={:?}", old_times[3], new_times[3]);
    }
}
