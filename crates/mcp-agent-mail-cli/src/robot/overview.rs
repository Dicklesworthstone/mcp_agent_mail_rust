//! Cold overview collection without SQL joins or per-project queries (GH#274).
//!
//! Keep the projections narrow: message bodies and reservation paths are not
//! needed. Each table is read once; message/recipient correlation is a hash
//! lookup, independent of the embedded SQL engine's join strategy.

use std::collections::HashMap;

use super::{
    ACK_OVERDUE_THRESHOLD_US, CliError, DbConn, OverviewProject,
    active_reservation_candidate_sql, has_file_reservation_release_ledger,
    has_file_reservations_released_ts_column, micros_ago, release_ledger_index,
};
use sqlmodel_core::{Row, Value};

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
/// existing release-ledger loader. Tests use this instead of timing budgets.
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

    // Preserve the already-fixed release-ledger path. Membership, not the
    // ledger timestamp's nullability, determines whether a release exists.
    let has_ledger = has_file_reservation_release_ledger(conn);
    let legacy = has_file_reservations_released_ts_column(conn);
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
        if !ledger.contains(integer(&row, "id")?) {
            project(&mut projects, integer(&row, "project_id")?).reservations += 1;
        }
    }

    let mut projects: Vec<_> = projects.into_values().collect();
    projects.sort_by(|left, right| left.slug.cmp(&right.slug));
    Ok((projects, work))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn execute(conn: &DbConn, sql: &str) {
        conn.execute_sync(sql, &[]).expect(sql);
    }

    fn fixture(legacy: bool, ledger: bool) -> (tempfile::TempDir, DbConn) {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("overview.sqlite3");
        let conn = DbConn::open_file(path.to_str().expect("UTF-8 path")).expect("open database");
        execute(&conn, "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL)");
        execute(&conn, "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL)");
        execute(&conn, "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
            importance TEXT, ack_required INTEGER, created_ts INTEGER)");
        execute(&conn, "CREATE TABLE message_recipients (message_id INTEGER NOT NULL,
            agent_id INTEGER NOT NULL, read_ts INTEGER, ack_ts INTEGER,
            PRIMARY KEY (message_id, agent_id))");
        let released_column = if legacy { ", released_ts INTEGER" } else { "" };
        execute(&conn, &format!("CREATE TABLE file_reservations (id INTEGER PRIMARY KEY,
            project_id INTEGER NOT NULL, created_ts INTEGER NOT NULL, expires_ts INTEGER NOT NULL
            {released_column})"));
        if ledger {
            execute(&conn, "CREATE TABLE file_reservation_releases (
                reservation_id INTEGER PRIMARY KEY, released_ts INTEGER)");
        }
        (dir, conn)
    }

    fn json(projects: &[OverviewProject]) -> serde_json::Value {
        serde_json::to_value(projects).expect("serialize overview")
    }

    #[test]
    fn linear_overview_matches_current_main_across_release_schemas() {
        for legacy in [false, true] {
            for ledger in [false, true] {
                let (_dir, conn) = fixture(legacy, ledger);
                let now = mcp_agent_mail_db::now_micros();
                let old = now - ACK_OVERDUE_THRESHOLD_US - 60_000_000;
                let future = now + 60_000_000;
                execute(&conn, "INSERT INTO projects VALUES (1, 'alpha'), (2, 'empty')");
                execute(&conn, "INSERT INTO agents VALUES (1, 77), (2, 77), (3, 1)");
                execute(&conn, &format!("INSERT INTO messages VALUES
                    (1, 1, 'urgent', 1, {old}), (2, 1, 'high', 0, {old}),
                    (3, 1, 'normal', 1, {old}), (4, 88, 'urgent', 1, {old}),
                    (5, 99, 'normal', 0, {old}), (6, 1, 'URGENT', 0, {old})"));
                execute(&conn, "INSERT INTO message_recipients VALUES
                    (1, 1, NULL, NULL), (1, 2, 0, NULL), (1, 3, NULL, 0),
                    (2, 1, NULL, NULL), (3, 1, 0, NULL), (4, 1, NULL, NULL),
                    (6, 1, NULL, NULL), (999, 1, NULL, NULL)");
                execute(&conn, &format!("INSERT INTO file_reservations
                    (id, project_id, created_ts, expires_ts) VALUES
                    (1, 1, {old}, {future}), (2, 66, {old}, {future}),
                    (3, 55, {old}, {old}), (4, 44, {old}, {future}),
                    (5, 33, {old}, {future})"));
                if ledger {
                    execute(&conn, "INSERT INTO file_reservation_releases VALUES (4, NULL)");
                }
                if legacy {
                    execute(&conn, &format!("UPDATE file_reservations SET released_ts = {now} WHERE id = 5"));
                }
                let actual = build(&conn).expect("linear overview");
                let reference = super::super::build_overview_reference(&conn).expect("reference overview");
                assert_eq!(json(&actual), json(&reference), "legacy={legacy} ledger={ledger}");
                let alpha = actual.iter().find(|row| row.slug == "alpha").expect("alpha");
                assert_eq!((alpha.unread, alpha.urgent, alpha.ack_overdue), (4, 3, 3));
                assert!(actual.iter().any(|row| row.slug == "[unknown-project-99]"));
                assert!(!actual.iter().any(|row| row.slug == "[unknown-project-55]"));
                assert_eq!(actual.iter().any(|row| row.slug == "[unknown-project-44]"), !ledger);
                assert_eq!(actual.iter().any(|row| row.slug == "[unknown-project-33]"), !legacy);
            }
        }
    }

    #[test]
    fn linear_overview_preserves_strict_time_boundaries_and_null_semantics() {
        let (_dir, conn) = fixture(false, true);
        let now = 10 * ACK_OVERDUE_THRESHOLD_US;
        let threshold = micros_ago(now, ACK_OVERDUE_THRESHOLD_US);
        execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
        execute(&conn, &format!("INSERT INTO messages VALUES
            (1, 1, 'high', 1, {}), (2, 1, 'urgent', 1, {threshold}),
            (3, 1, 'normal', 1, {}), (4, 1, NULL, NULL, NULL)", threshold - 1, threshold + 1));
        execute(&conn, "INSERT INTO message_recipients VALUES
            (1, 1, NULL, NULL), (1, 2, 0, NULL), (1, 3, NULL, 0),
            (1, 4, 0, 0), (2, 1, NULL, NULL), (3, 1, NULL, NULL), (4, 1, NULL, NULL)");
        execute(&conn, &format!("INSERT INTO file_reservations VALUES
            (1, 1, 0, {now}), (2, 1, 0, {}), (3, 1, 0, {})", now + 1, now - 1));
        let (rows, work) = build_at(&conn, now).expect("overview at boundary");
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].unread, rows[0].urgent, rows[0].ack_overdue, rows[0].reservations), (5, 3, 2, 1));
        assert_eq!(work.queries, 5);
        assert_eq!(work.rows, [1, 0, 4, 6, 1]);
    }

    fn seed_scale(conn: &DbConn, projects: usize, per_project: usize) {
        let now = mcp_agent_mail_db::now_micros();
        let old = now - ACK_OVERDUE_THRESHOLD_US - 60_000_000;
        execute(conn, "BEGIN");
        for p in 1..=projects {
            execute(conn, &format!("INSERT INTO projects VALUES ({p}, 'project-{p:04}')"));
            execute(conn, &format!("INSERT INTO agents VALUES ({p}, {p})"));
            for m in 1..=per_project {
                let id = (p - 1) * per_project + m;
                execute(conn, &format!("INSERT INTO messages VALUES ({id}, {p}, 'high', 1, {old})"));
                execute(conn, &format!("INSERT INTO message_recipients VALUES
                    ({id}, 1, NULL, NULL), ({id}, 2, 0, NULL), ({id}, 3, 0, 0)"));
                execute(conn, &format!("INSERT INTO file_reservations VALUES ({id}, {p}, {old}, {old})"));
            }
        }
        execute(conn, "COMMIT");
    }

    #[test]
    fn linear_overview_work_is_bounded_by_input_rows_not_project_times_recipients() {
        for projects in [1, 10, 50] {
            let (_dir, conn) = fixture(false, false);
            seed_scale(&conn, projects, 10);
            let (rows, work) = build_at(&conn, mcp_agent_mail_db::now_micros()).expect("overview");
            assert_eq!(work.queries, 5);
            assert_eq!(work.rows, [projects, projects, projects * 10, projects * 20, 0]);
            assert_eq!(rows.len(), projects);
            for row in rows {
                assert_eq!((row.unread, row.urgent, row.ack_overdue, row.reservations), (10, 10, 20, 0));
            }
        }
        for sql in [PROJECTS_SQL, AGENTS_SQL, MESSAGES_SQL, RECIPIENTS_SQL] {
            assert!(!sql.contains("JOIN"));
            assert!(!sql.contains("GROUP BY"));
            assert!(!sql.contains("DISTINCT"));
        }
    }

    #[test]
    #[ignore = "native-engine benchmark; run explicitly in release mode with --nocapture"]
    fn benchmark_linear_overview_against_current_main() {
        let (_dir, conn) = fixture(false, false);
        let per_project = std::env::var("AM_OVERVIEW_BENCH_MESSAGES_PER_PROJECT")
            .map_or(480, |value| value.parse().expect("positive messages-per-project count"));
        assert!(per_project > 0);
        seed_scale(&conn, 50, per_project);
        let baseline = super::super::build_overview_reference(&conn).expect("reference");
        assert_eq!(json(&build(&conn).expect("linear")), json(&baseline));
        let mut old = Vec::new();
        let mut new = Vec::new();
        for iteration in 0..6 {
            // Alternate order to reduce filesystem/page-cache ordering bias.
            for linear in [iteration % 2 == 0, iteration % 2 != 0] {
                let start = Instant::now();
                let result = if linear { build(&conn) } else { super::super::build_overview_reference(&conn) };
                let elapsed = start.elapsed();
                assert_eq!(json(&result.expect("benchmark result")), json(&baseline));
                if linear { new.push(elapsed); } else { old.push(elapsed); }
            }
        }
        old.sort_unstable();
        new.sort_unstable();
        let reference = old[old.len() / 2];
        let linear = new[new.len() / 2];
        eprintln!("native DbConn live-build benchmark (not CLI startup), 50 projects / {} messages / {} recipients / {} expired reservations: reference_median={reference:?}, linear_median={linear:?}, speedup={:.3}", 50 * per_project, 150 * per_project, 50 * per_project, reference.as_secs_f64() / linear.as_secs_f64());
        // Opt-in benchmark gate, never a wall-clock assertion in regular tests.
        if std::env::var("AM_OVERVIEW_BENCH_REQUIRE_SPEEDUP").as_deref() == Ok("1") {
            assert!(linear < reference, "linear scan must improve the native reference on this fixture");
        }
    }
}
