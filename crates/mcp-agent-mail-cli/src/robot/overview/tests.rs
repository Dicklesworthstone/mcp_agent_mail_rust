use super::*;
use std::time::Instant;

fn execute(conn: &DbConn, sql: &str) {
    conn.execute_sync(sql, &[]).expect(sql);
}

fn fixture(legacy: bool, ledger: bool) -> (tempfile::TempDir, DbConn) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("overview.sqlite3");
    let conn = DbConn::open_file(path.to_str().expect("UTF-8 path")).expect("open database");
    execute(
        &conn,
        "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL)",
    );
    execute(
        &conn,
        "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL)",
    );
    execute(
        &conn,
        "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
        importance TEXT, ack_required INTEGER, created_ts INTEGER)",
    );
    execute(
        &conn,
        "CREATE TABLE message_recipients (message_id INTEGER NOT NULL,
        agent_id INTEGER NOT NULL, read_ts INTEGER, ack_ts INTEGER,
        PRIMARY KEY (message_id, agent_id))",
    );
    let released_column = if legacy { ", released_ts INTEGER" } else { "" };
    execute(
        &conn,
        &format!(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY,
        project_id INTEGER NOT NULL, created_ts INTEGER NOT NULL, expires_ts INTEGER NOT NULL
        {released_column})"
        ),
    );
    if ledger {
        execute(
            &conn,
            "CREATE TABLE file_reservation_releases (
            reservation_id INTEGER PRIMARY KEY, released_ts INTEGER)",
        );
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
            let future = now + 86_400_000_000;
            execute(
                &conn,
                "INSERT INTO projects VALUES (1, 'alpha'), (2, 'empty')",
            );
            execute(&conn, "INSERT INTO agents VALUES (1, 77), (2, 77), (3, 1)");
            execute(
                &conn,
                &format!(
                    "INSERT INTO messages VALUES
                (1, 1, 'urgent', 1, {old}), (2, 1, 'high', 0, {old}),
                (3, 1, 'normal', 1, {old}), (4, 88, 'urgent', 1, {old}),
                (5, 99, 'normal', 0, {old}), (6, 1, 'URGENT', 0, {old})"
                ),
            );
            execute(
                &conn,
                "INSERT INTO message_recipients VALUES
                (1, 1, NULL, NULL), (1, 2, 0, NULL), (1, 3, NULL, 0),
                (2, 1, NULL, NULL), (3, 1, 0, NULL), (4, 1, NULL, NULL),
                (6, 1, NULL, NULL), (999, 1, NULL, NULL)",
            );
            execute(
                &conn,
                &format!(
                    "INSERT INTO file_reservations
                (id, project_id, created_ts, expires_ts) VALUES
                (1, 1, {old}, {future}), (2, 66, {old}, {future}),
                (3, 55, {old}, {old}), (4, 44, {old}, {future}),
                (5, 33, {old}, {future})"
                ),
            );
            if ledger {
                execute(
                    &conn,
                    "INSERT INTO file_reservation_releases VALUES (4, NULL)",
                );
            }
            if legacy {
                execute(
                    &conn,
                    &format!("UPDATE file_reservations SET released_ts = {now} WHERE id = 5"),
                );
            }
            let actual = build(&conn).expect("linear overview");
            let reference = reference(&conn).expect("reference overview");
            assert_eq!(
                json(&actual),
                json(&reference),
                "legacy={legacy} ledger={ledger}"
            );
            let alpha = actual
                .iter()
                .find(|row| row.slug == "alpha")
                .expect("alpha");
            assert_eq!((alpha.unread, alpha.urgent, alpha.ack_overdue), (4, 3, 3));
            assert!(actual.iter().any(|row| row.slug == "[unknown-project-99]"));
            assert!(!actual.iter().any(|row| row.slug == "[unknown-project-55]"));
            assert_eq!(
                actual.iter().any(|row| row.slug == "[unknown-project-44]"),
                !ledger
            );
            assert_eq!(
                actual.iter().any(|row| row.slug == "[unknown-project-33]"),
                !legacy
            );
        }
    }
}

#[test]
fn linear_overview_preserves_strict_time_boundaries_and_null_semantics() {
    let (_dir, conn) = fixture(false, true);
    let now = 10 * ACK_OVERDUE_THRESHOLD_US;
    let threshold = micros_ago(now, ACK_OVERDUE_THRESHOLD_US);
    execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
    execute(
        &conn,
        &format!(
            "INSERT INTO messages VALUES
        (1, 1, 'high', 1, {}), (2, 1, 'urgent', 1, {threshold}),
        (3, 1, 'normal', 1, {}), (4, 1, NULL, NULL, NULL)",
            threshold - 1,
            threshold + 1
        ),
    );
    execute(
        &conn,
        "INSERT INTO message_recipients VALUES
        (1, 1, NULL, NULL), (1, 2, 0, NULL), (1, 3, NULL, 0),
        (1, 4, 0, 0), (2, 1, NULL, NULL), (3, 1, NULL, NULL), (4, 1, NULL, NULL)",
    );
    execute(
        &conn,
        &format!(
            "INSERT INTO file_reservations VALUES
        (1, 1, 0, {now}), (2, 1, 0, {}), (3, 1, 0, {})",
            now + 1,
            now - 1
        ),
    );
    let (rows, work) = build_at(&conn, now).expect("overview at boundary");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].unread,
            rows[0].urgent,
            rows[0].ack_overdue,
            rows[0].reservations
        ),
        (5, 3, 2, 1)
    );
    assert_eq!(work.queries, 7);
    assert_eq!(work.rows, [1, 0, 4, 6, 1]);
}

// Frozen live-build query shape from f8ca6f74 (before the linear collector).
// No cache and no timing assumptions: the benchmark compares the collectors.
fn reference(conn: &DbConn) -> Result<Vec<OverviewProject>, CliError> {
    let now = mcp_agent_mail_db::now_micros();
    let mut projects = HashMap::new();
    let sql = "WITH live_projects AS (
         SELECT p.id AS project_id, p.slug FROM projects p
     ), orphan_project_ids AS (
         SELECT DISTINCT m.project_id AS raw_project_id FROM messages m
         LEFT JOIN projects p ON p.id = m.project_id WHERE p.id IS NULL
         UNION
         SELECT DISTINCT a.project_id AS raw_project_id FROM agents a
         LEFT JOIN projects p ON p.id = a.project_id WHERE p.id IS NULL
     )
     SELECT project_id AS id, slug FROM live_projects
     UNION ALL SELECT raw_project_id AS id,
         '[unknown-project-' || raw_project_id || ']' AS slug
     FROM orphan_project_ids";
    for row in conn.query_sync(sql, &[]).expect("reference inventory") {
        let id = integer(&row, "id")?;
        project(&mut projects, id).slug = row.get_named("slug").expect("reference slug");
    }
    let ledger = release_ledger_index(conn, has_file_reservation_release_ledger(conn)?)?;
    let predicate =
        active_reservation_candidate_sql(has_file_reservations_released_ts_column(conn)?, "fr");
    for row in conn
        .query_sync(
            &format!(
                "SELECT fr.id, fr.project_id FROM file_reservations fr
         WHERE ({predicate}) AND fr.expires_ts > ?"
            ),
            &[Value::BigInt(now)],
        )
        .expect("reference reservations")
    {
        if !ledger.contains(&integer(&row, "id")?) {
            project(&mut projects, integer(&row, "project_id")?).reservations += 1;
        }
    }
    let sql = "SELECT m.project_id AS project_id,
        SUM(CASE WHEN mr.read_ts IS NULL THEN 1 ELSE 0 END) AS unread,
        SUM(CASE WHEN mr.read_ts IS NULL AND m.importance IN ('urgent', 'high')
                 THEN 1 ELSE 0 END) AS urgent,
        SUM(CASE WHEN m.ack_required = 1 AND mr.ack_ts IS NULL AND m.created_ts < ?
                 THEN 1 ELSE 0 END) AS ack_overdue
        FROM message_recipients mr JOIN messages m ON m.id = mr.message_id
        GROUP BY m.project_id";
    for row in conn
        .query_sync(
            sql,
            &[Value::BigInt(micros_ago(now, ACK_OVERDUE_THRESHOLD_US))],
        )
        .expect("reference recipient aggregate")
    {
        let counts = project(&mut projects, integer(&row, "project_id")?);
        counts.unread = usize::try_from(integer(&row, "unread")?).expect("nonnegative unread");
        counts.urgent = usize::try_from(integer(&row, "urgent")?).expect("nonnegative urgent");
        counts.ack_overdue =
            usize::try_from(integer(&row, "ack_overdue")?).expect("nonnegative overdue");
    }
    let mut rows: Vec<_> = projects.into_values().collect();
    rows.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(rows)
}

#[test]
fn cold_overview_recomputes_time_boundaries_without_a_write() {
    let (_dir, conn) = fixture(false, true);
    let now = ACK_OVERDUE_THRESHOLD_US * 10;
    let threshold = micros_ago(now, ACK_OVERDUE_THRESHOLD_US);
    execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
    execute(
        &conn,
        &format!("INSERT INTO messages VALUES (1, 1, 'high', 1, {threshold})"),
    );
    execute(
        &conn,
        "INSERT INTO message_recipients VALUES (1, 1, NULL, NULL)",
    );
    execute(
        &conn,
        &format!(
            "INSERT INTO file_reservations VALUES (1, 1, 0, {})",
            now + 1
        ),
    );
    let (before, _) = build_at(&conn, now).expect("before deadline");
    let (after, _) = build_at(&conn, now + 1).expect("after deadline");
    assert_eq!((before[0].ack_overdue, before[0].reservations), (0, 1));
    assert_eq!((after[0].ack_overdue, after[0].reservations), (1, 0));
}

#[test]
fn cold_overview_works_read_only_and_preserves_the_callers_transaction() {
    let (_dir, conn) = fixture(false, false);
    execute(&conn, "BEGIN");
    execute(&conn, "INSERT INTO projects VALUES (1, 'not-committed')");
    assert_eq!(build(&conn).expect("nested read").len(), 1);
    execute(&conn, "ROLLBACK");
    execute(&conn, "PRAGMA query_only = ON");
    assert!(build(&conn).expect("query-only read").is_empty());
}

#[test]
fn cold_overview_releases_its_savepoint_after_a_query_failure() {
    let (_dir, conn) = fixture(false, false);
    execute(&conn, "DROP TABLE message_recipients");
    assert!(build(&conn).is_err());
    assert!(
        conn.execute_sync("RELEASE robot_overview_read", &[])
            .is_err()
    );
    execute(
        &conn,
        "CREATE TABLE message_recipients (message_id INTEGER, read_ts INTEGER, ack_ts INTEGER)",
    );
    assert!(
        build(&conn)
            .expect("read after failed collection")
            .is_empty()
    );
}

#[test]
fn cold_overview_does_not_hide_mutations_below_a_max_timestamp() {
    let (_dir, conn) = fixture(false, true);
    let now = ACK_OVERDUE_THRESHOLD_US * 10;
    execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
    execute(
        &conn,
        "INSERT INTO messages VALUES (1, 1, 'high', 1, 0), (2, 1, 'normal', 0, 0)",
    );
    execute(
        &conn,
        "INSERT INTO message_recipients VALUES (1, 1, NULL, NULL), (2, 1, 999, 999)",
    );
    let (before, _) = build_at(&conn, now).expect("before read");
    execute(
        &conn,
        "UPDATE message_recipients SET read_ts = 1, ack_ts = 1 WHERE message_id = 1",
    );
    let (after, _) = build_at(&conn, now).expect("after read below existing max");
    assert_eq!(
        (before[0].unread, before[0].urgent, before[0].ack_overdue),
        (1, 1, 1)
    );
    assert_eq!(
        (after[0].unread, after[0].urgent, after[0].ack_overdue),
        (0, 0, 0)
    );
    execute(
        &conn,
        &format!(
            "INSERT INTO file_reservations VALUES (1, 1, 0, {})",
            now + 1
        ),
    );
    assert_eq!(build_at(&conn, now).expect("grant").0[0].reservations, 1);
    execute(
        &conn,
        "INSERT INTO file_reservation_releases VALUES (1, NULL)",
    );
    assert_eq!(build_at(&conn, now).expect("release").0[0].reservations, 0);
}

#[test]
fn live_overview_renderer_preserves_counts_and_full_envelopes() {
    let rows = vec![
        OverviewProject {
            slug: "alpha".into(),
            unread: 3,
            urgent: 2,
            ack_overdue: 1,
            reservations: 4,
        },
        OverviewProject {
            slug: "empty".into(),
            unread: 0,
            urgent: 0,
            ack_overdue: 0,
            reservations: 0,
        },
    ];
    let counts: serde_json::Value =
        serde_json::from_str(&render(&rows, true, OutputFormat::Json).expect("counts"))
            .expect("counts JSON");
    let full: serde_json::Value =
        serde_json::from_str(&render(&rows, false, OutputFormat::Json).expect("full"))
            .expect("full JSON");
    assert_eq!(counts["_meta"]["command"], "robot overview");
    assert_eq!(counts["project_count"], 2);
    assert_eq!(counts["unread"], 3);
    assert_eq!(counts["urgent"], 2);
    assert_eq!(counts["ack_overdue"], 1);
    assert!(counts.get("projects").is_none());
    assert_eq!(full["project_count"], 2);
    assert_eq!(full["projects"], json(&rows));
    assert!(
        !render(&rows, true, OutputFormat::Toon)
            .expect("TOON counts")
            .is_empty()
    );
    let empty: serde_json::Value =
        serde_json::from_str(&render(&[], true, OutputFormat::Json).expect("empty"))
            .expect("empty JSON");
    assert_eq!(empty["project_count"], 0);
    assert_eq!(empty["unread"], 0);
    assert_eq!(empty["urgent"], 0);
    assert_eq!(empty["ack_overdue"], 0);
}

fn seed_scale(conn: &DbConn, projects: usize, per_project: usize) {
    let now = mcp_agent_mail_db::now_micros();
    let old = now - ACK_OVERDUE_THRESHOLD_US - 60_000_000;
    execute(conn, "BEGIN");
    for p in 1..=projects {
        execute(
            conn,
            &format!("INSERT INTO projects VALUES ({p}, 'project-{p:04}')"),
        );
        execute(conn, &format!("INSERT INTO agents VALUES ({p}, {p})"));
        for m in 1..=per_project {
            let id = (p - 1) * per_project + m;
            execute(
                conn,
                &format!("INSERT INTO messages VALUES ({id}, {p}, 'high', 1, {old})"),
            );
            execute(
                conn,
                &format!(
                    "INSERT INTO message_recipients VALUES
                ({id}, 1, NULL, NULL), ({id}, 2, 0, NULL), ({id}, 3, 0, 0)"
                ),
            );
            execute(
                conn,
                &format!("INSERT INTO file_reservations VALUES ({id}, {p}, {old}, {old})"),
            );
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
        let pages = |count: usize| count / OVERVIEW_PAGE_ROWS + 1;
        let recipient_batches = (projects * 20).div_ceil(OVERVIEW_PAGE_ROWS);
        assert_eq!(
            work.queries,
            2 * pages(projects)
                + pages(projects * 10)
                + pages(projects * 20)
                + 1
                + recipient_batches
        );
        assert!(work.peak_query_rows <= OVERVIEW_PAGE_ROWS);
        assert!(work.peak_message_keys <= OVERVIEW_PAGE_ROWS);
        assert_eq!(
            work.rows,
            [projects, projects, projects * 10, projects * 20, 0]
        );
        assert_eq!(rows.len(), projects);
        for row in rows {
            assert_eq!(
                (row.unread, row.urgent, row.ack_overdue, row.reservations),
                (10, 10, 20, 0)
            );
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
        .map_or(480, |value| {
            value.parse().expect("positive messages-per-project count")
        });
    assert!(per_project > 0);
    seed_scale(&conn, 50, per_project);
    let baseline = reference(&conn).expect("reference");
    assert_eq!(json(&build(&conn).expect("linear")), json(&baseline));
    let mut old = Vec::new();
    let mut new = Vec::new();
    for iteration in 0..6 {
        // Alternate order to reduce filesystem/page-cache ordering bias.
        for linear in [iteration % 2 == 0, iteration % 2 != 0] {
            let start = Instant::now();
            let result = if linear {
                build(&conn)
            } else {
                reference(&conn)
            };
            let elapsed = start.elapsed();
            assert_eq!(json(&result.expect("benchmark result")), json(&baseline));
            if linear {
                new.push(elapsed);
            } else {
                old.push(elapsed);
            }
        }
    }
    old.sort_unstable();
    new.sort_unstable();
    let reference = old[old.len() / 2];
    let linear = new[new.len() / 2];
    eprintln!(
        "native DbConn live-build benchmark (not CLI startup), 50 projects / {} messages / {} recipients / {} expired reservations: reference_median={reference:?}, linear_median={linear:?}, speedup={:.3}",
        50 * per_project,
        150 * per_project,
        50 * per_project,
        reference.as_secs_f64() / linear.as_secs_f64()
    );
    // Opt-in benchmark gate, never a wall-clock assertion in regular tests.
    if std::env::var("AM_OVERVIEW_BENCH_REQUIRE_SPEEDUP").as_deref() == Ok("1") {
        assert!(
            linear < reference,
            "linear scan must improve the native reference on this fixture"
        );
    }
}

mod bounded;
