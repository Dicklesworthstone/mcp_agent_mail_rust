//! Reservation presence for `overview --counts`, not reservation multiplicity.
//!
//! Registered projects and message/agent orphans are already in the inventory.
//! Their reservations cannot change the counts payload. Only a previously
//! unknown project with at least one active, unreleased reservation can do so.
//! All reads use the caller's overview snapshot; no schema or data is written.

use super::*;

const RESERVATION_PROJECTS_SQL: &str = "SELECT project_id FROM file_reservations";
const INITIAL_PAGES: usize = 4;
const MAX_INDEX_QUERIES: usize = 64;
const MAX_ORPHAN_PAGES: usize = 4;

pub(super) fn collect(
    conn: &DbConn,
    now_us: i64,
    projects: &mut HashMap<i64, OverviewProject>,
    work: &mut ScanWork,
) -> Result<(), CliError> {
    let ledger = has_file_reservation_release_ledger(conn)?;
    let legacy = has_file_reservations_released_ts_column(conn)?;
    let predicate = active_reservation_candidate_sql(legacy, "fr");
    let mut pages = 0;
    // Small/empty active sets need no project-index walk. This is actual
    // collection, not a sample used to infer that unseen projects do not exist.
    let complete = scan_reservation_pages_while(conn, work, &predicate, now_us, |rows, work| {
        add_orphans(conn, rows, ledger, projects, work)?;
        pages += 1;
        Ok(pages < INITIAL_PAGES)
    })?;
    if complete {
        return Ok(());
    }

    // Cap strategy overhead on very broad or sparse project inventories.
    // Missing/partial indexes must not create false zeroes in read-only copies.
    if projects.len() < MAX_INDEX_QUERIES
        && let Some(index) = project_inventory_index(conn, "file_reservations")?
        && seek_unknown_projects(conn, now_us, &predicate, ledger, &index, projects, work)?
    {
        return Ok(());
    }
    // A budget is a strategy limit, NEVER a result limit. Restart the exact
    // bounded scan if discovery was inconclusive. Existing project membership
    // makes rereading the initial pages idempotent; no counter is double-added.
    scan_reservation_pages(conn, work, &predicate, now_us, |rows, work| {
        add_orphans(conn, rows, ledger, projects, work)
    })
}

fn add_orphans(
    conn: &DbConn,
    rows: &[Row],
    ledger: bool,
    projects: &mut HashMap<i64, OverviewProject>,
    work: &mut ScanWork,
) -> Result<(), CliError> {
    let mut candidates = Vec::new();
    for row in rows {
        let project_id = integer(row, "project_id")?;
        if !projects.contains_key(&project_id) {
            candidates.push((integer(row, "id")?, project_id));
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let mut released = HashSet::new();
    if ledger {
        let ids: Vec<_> = candidates.iter().map(|&(id, _)| id).collect();
        let rows = lookup_ids(conn, RELEASE_LOOKUP_SQL, "reservation_id", &[], &ids, work)?;
        work.release_lookup_rows += rows.len();
        for row in rows {
            released.insert(integer(&row, "reservation_id")?);
        }
    }
    work.peak_release_keys = work.peak_release_keys.max(released.len());
    for (id, project_id) in candidates {
        // Ledger membership releases a reservation even with NULL/zero time.
        if !released.contains(&id) {
            project(projects, project_id);
        }
    }
    Ok(())
}

fn empty_integer_gap(lower: Option<i64>, upper: Option<i64>) -> bool {
    match (lower, upper) {
        (None, Some(i64::MIN)) | (Some(i64::MAX), None) => true,
        (Some(lower), Some(upper)) => lower.checked_add(1) == Some(upper),
        _ => false,
    }
}

/// Probe only gaps between already-known integer project IDs. A contiguous
/// registered inventory needs just the lower/upper gap seeks, regardless of
/// its reservation history. No SQL anti-join, NOT IN, DISTINCT or GROUP BY.
/// Unseen IDs are checked for active/unreleased presence before being added.
/// False means the strategy budget ran out and the caller must finish by scan.
fn seek_unknown_projects(
    conn: &DbConn,
    now_us: i64,
    predicate: &str,
    ledger: bool,
    index: &str,
    projects: &mut HashMap<i64, OverviewProject>,
    work: &mut ScanWork,
) -> Result<bool, CliError> {
    let mut known: Vec<_> = projects.keys().copied().collect();
    known.sort_unstable();
    let index = quoted_identifier(index);
    let started = work.queries;
    let mut lower = None;
    for upper in known.into_iter().map(Some).chain(std::iter::once(None)) {
        if empty_integer_gap(lower, upper) {
            lower = upper;
            continue;
        }
        let mut after = lower;
        loop {
            if work.queries - started >= MAX_INDEX_QUERIES {
                return Ok(false);
            }
            let mut params = Vec::new();
            let mut conditions = Vec::new();
            if let Some(id) = after {
                conditions.push("project_id > ?");
                params.push(Value::BigInt(id));
            }
            if let Some(id) = upper {
                conditions.push("project_id < ?");
                params.push(Value::BigInt(id));
            }
            let filter = if conditions.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", conditions.join(" AND "))
            };
            let sql = format!(
                "{RESERVATION_PROJECTS_SQL} INDEXED BY {index} \
                {filter} ORDER BY project_id LIMIT 1"
            );
            let rows = bounded_query(conn, &sql, &params, work)?;
            work.reservation_project_rows += rows.len();
            if rows.is_empty() {
                break;
            }
            if rows.len() != 1 {
                return Err(CliError::Other(
                    "overview project seek exceeded one key".to_string(),
                ));
            }
            let id = integer(&rows[0], "project_id")?;
            if after.is_some_and(|value| id <= value) || upper.is_some_and(|value| id >= value) {
                return Err(CliError::Other(
                    "overview project seek left its gap".to_string(),
                ));
            }
            after = Some(id);
            // id was decoded as i64, not interpolated from a user identifier.
            let scoped = format!("({predicate}) AND fr.project_id = {id}");
            let mut pages = 0;
            let complete =
                scan_reservation_pages_while(conn, work, &scoped, now_us, |rows, work| {
                    for row in rows {
                        if integer(row, "project_id")? != id {
                            return Err(CliError::Other(
                                "overview reservation escaped its project".to_string(),
                            ));
                        }
                    }
                    add_orphans(conn, rows, ledger, projects, work)?;
                    pages += 1;
                    Ok(!projects.contains_key(&id)
                        && pages < MAX_ORPHAN_PAGES
                        && work.queries - started < MAX_INDEX_QUERIES)
                })?;
            if !complete && !projects.contains_key(&id) {
                return Ok(false);
            }
        }
        lower = upper;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const NOW: i64 = 18_000_000_000;

    fn execute(conn: &DbConn, sql: &str) {
        conn.execute_sync(sql, &[]).expect(sql);
    }

    fn fixture(legacy: bool, ledger: bool) -> (tempfile::TempDir, DbConn) {
        let dir = tempfile::tempdir().expect("counts directory");
        let conn = DbConn::open_file(dir.path().join("counts.sqlite3").to_str().unwrap())
            .expect("native counts database");
        for sql in [
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL)",
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL)",
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, importance TEXT, ack_required INTEGER, created_ts INTEGER)",
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, read_ts INTEGER, ack_ts INTEGER)",
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, expires_ts INTEGER NOT NULL)",
            "CREATE INDEX expiry ON file_reservations(expires_ts)",
        ] {
            execute(&conn, sql);
        }
        if legacy {
            execute(
                &conn,
                "ALTER TABLE file_reservations ADD COLUMN released_ts INTEGER",
            );
        }
        if ledger {
            execute(
                &conn,
                "CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY, released_ts INTEGER)",
            );
        }
        (dir, conn)
    }

    fn index(conn: &DbConn) {
        execute(
            conn,
            "CREATE INDEX project_expiry ON file_reservations(project_id, expires_ts)",
        );
    }

    fn seed(conn: &DbConn, count: usize, project_id: i64, released: bool) {
        execute(conn, "BEGIN");
        for start in (1..=count).step_by(OVERVIEW_PAGE_ROWS) {
            let end = (start + OVERVIEW_PAGE_ROWS - 1).min(count);
            let values: Vec<_> = (start..=end)
                .map(|id| format!("({id}, {project_id}, {})", NOW + 1))
                .collect();
            execute(
                conn,
                &format!(
                    "INSERT INTO file_reservations (id, project_id, expires_ts) VALUES {}",
                    values.join(",")
                ),
            );
            if released {
                let values: Vec<_> = (start..=end).map(|id| format!("({id}, NULL)")).collect();
                execute(
                    conn,
                    &format!(
                        "INSERT INTO file_reservation_releases VALUES {}",
                        values.join(",")
                    ),
                );
            }
        }
        execute(conn, "COMMIT");
    }

    fn payload(rows: &[OverviewProject]) -> serde_json::Value {
        let mut json: serde_json::Value =
            serde_json::from_str(&render(rows, true, OutputFormat::Json).expect("counts render"))
                .expect("counts JSON");
        json.as_object_mut().unwrap().remove("_meta");
        json
    }

    fn compare(conn: &DbConn, now: i64) -> (ScanWork, ScanWork) {
        let (full, old) = build_at(conn, now).expect("full overview");
        let (counts, new) = build_at_mode(conn, now, true).expect("counts-only overview");
        assert_eq!(payload(&counts), payload(&full));
        assert_eq!(
            counts.iter().map(|row| &row.slug).collect::<Vec<_>>(),
            full.iter().map(|row| &row.slug).collect::<Vec<_>>()
        );
        assert!(counts.iter().all(|row| row.reservations == 0));
        assert!(new.peak_query_rows <= OVERVIEW_PAGE_ROWS);
        assert!(new.peak_release_keys <= OVERVIEW_PAGE_ROWS);
        (old, new)
    }

    #[test]
    fn registered_reservation_history_does_not_grow_counts_work() {
        for count in [OVERVIEW_PAGE_ROWS * 8, OVERVIEW_PAGE_ROWS * 24] {
            let (_dir, conn) = fixture(false, true);
            index(&conn);
            execute(
                &conn,
                "INSERT INTO projects VALUES (1, 'alpha'), (2, 'empty')",
            );
            execute(&conn, "INSERT INTO messages VALUES (1, 1, 'urgent', 1, 0)");
            execute(
                &conn,
                "INSERT INTO message_recipients VALUES (1, 1, NULL, NULL), (1, 2, 0, NULL)",
            );
            seed(&conn, count, 1, true);
            let (old, new) = compare(&conn, NOW);
            assert_eq!(old.rows[4], count);
            assert_eq!(old.release_lookup_rows, count);
            assert_eq!(new.rows[4], INITIAL_PAGES * OVERVIEW_PAGE_ROWS);
            assert_eq!(new.reservation_project_rows, 0);
            assert_eq!(new.release_lookup_rows, 0);
            assert!(new.queries < old.queries, "old={old:?} new={new:?}");
        }
    }

    #[test]
    fn small_or_expired_active_sets_do_not_probe_the_project_index() {
        let (_dir, conn) = fixture(false, true);
        index(&conn);
        execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
        seed(&conn, OVERVIEW_PAGE_ROWS + 1, 1, true);
        let (_, work) = compare(&conn, NOW);
        assert_eq!(work.rows[4], OVERVIEW_PAGE_ROWS + 1);
        assert_eq!(work.reservation_project_rows, 0);
        assert_eq!(work.release_lookup_rows, 0);
        let (_, work) = compare(&conn, NOW + 1);
        assert_eq!(work.rows[4], 0);
        assert_eq!(work.release_lookup_rows, 0);
    }

    #[test]
    fn counts_keep_agent_message_and_active_reservation_orphans_only() {
        for legacy in [false, true] {
            for ledger in [false, true] {
                let (_dir, conn) = fixture(legacy, ledger);
                index(&conn);
                execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
                execute(&conn, "INSERT INTO agents VALUES (1, 7)");
                execute(&conn, "INSERT INTO messages VALUES (1, 8, 'normal', 0, 0)");
                seed(&conn, OVERVIEW_PAGE_ROWS * 5, 1, false);
                execute(
                    &conn,
                    &format!(
                        "INSERT INTO file_reservations (id, project_id, expires_ts) VALUES
                    (9001, 2, {}), (9002, 3, {}), (9003, 4, {}), (9004, 5, {}), (9005, 6, {})",
                        NOW + 1,
                        NOW,
                        NOW + 1,
                        NOW + 1,
                        NOW + 1
                    ),
                );
                if ledger {
                    execute(
                        &conn,
                        "INSERT INTO file_reservation_releases VALUES (9003, NULL), (9004, 0)",
                    );
                }
                if legacy {
                    execute(
                        &conn,
                        "UPDATE file_reservations SET released_ts = 1 WHERE id = 9005",
                    );
                }
                compare(&conn, NOW);
                let rows = build_at_mode(&conn, NOW, true).unwrap().0;
                assert!(rows.iter().any(|row| row.slug == "[unknown-project-2]"));
                assert!(!rows.iter().any(|row| row.slug == "[unknown-project-3]"));
                for id in [4, 5] {
                    assert_eq!(
                        rows.iter()
                            .any(|row| row.slug == format!("[unknown-project-{id}]")),
                        !ledger
                    );
                }
                assert_eq!(
                    rows.iter().any(|row| row.slug == "[unknown-project-6]"),
                    !legacy
                );
                compare(&conn, NOW + 1);
            }
        }
    }

    #[test]
    fn released_orphan_prefix_cannot_hide_a_later_unreleased_reservation() {
        let (_dir, conn) = fixture(false, true);
        index(&conn);
        seed(
            &conn,
            OVERVIEW_PAGE_ROWS * (INITIAL_PAGES + MAX_ORPHAN_PAGES + 2),
            77,
            true,
        );
        execute(
            &conn,
            &format!(
                "INSERT INTO file_reservations VALUES (99999, 77, {})",
                NOW + 1
            ),
        );
        let (_, work) = compare(&conn, NOW);
        assert!(work.reservation_project_rows > 0);
        assert_eq!(build_at_mode(&conn, NOW, true).unwrap().0.len(), 1);
        execute(
            &conn,
            "INSERT INTO file_reservation_releases VALUES (99999, NULL)",
        );
        compare(&conn, NOW);
        assert!(build_at_mode(&conn, NOW, true).unwrap().0.is_empty());
    }

    #[test]
    fn missing_partial_or_nonleading_indexes_use_complete_fallback() {
        for ddl in [
            None,
            Some("CREATE INDEX unsuitable ON file_reservations(project_id) WHERE project_id = 1"),
            Some("CREATE INDEX unsuitable ON file_reservations(expires_ts, project_id)"),
            Some("CREATE INDEX unsuitable ON file_reservations((project_id + 0))"),
        ] {
            let (_dir, conn) = fixture(false, true);
            if let Some(ddl) = ddl {
                execute(&conn, ddl);
            }
            execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
            seed(&conn, OVERVIEW_PAGE_ROWS * 5, 1, true);
            execute(
                &conn,
                &format!(
                    "INSERT INTO file_reservations VALUES (99999, 77, {})",
                    NOW + 1
                ),
            );
            let (_, work) = compare(&conn, NOW);
            assert_eq!(work.reservation_project_rows, 0);
            assert!(work.rows[4] > OVERVIEW_PAGE_ROWS * 5);
            assert_eq!(work.release_lookup_rows, 0);
        }
    }

    #[test]
    fn gap_seeks_preserve_signed_extremes_and_quoted_descending_indexes() {
        let (_dir, conn) = fixture(false, false);
        execute(
            &conn,
            "CREATE INDEX \"project\"\"order\" ON file_reservations(project_id DESC)",
        );
        execute(
            &conn,
            "INSERT INTO projects VALUES (-1, 'left'), (0, 'middle'), (1, 'right')",
        );
        seed(&conn, OVERVIEW_PAGE_ROWS * 5, 0, false);
        execute(
            &conn,
            &format!(
                "INSERT INTO file_reservations VALUES (99998, {}, {}), (99999, {}, {})",
                i64::MIN,
                NOW + 1,
                i64::MAX,
                NOW + 1
            ),
        );
        compare(&conn, NOW);
        let rows = build_at_mode(&conn, NOW, true).unwrap().0;
        assert_eq!(rows.len(), 5);
        assert!(empty_integer_gap(None, Some(i64::MIN)));
        assert!(empty_integer_gap(Some(i64::MAX), None));
        assert!(empty_integer_gap(Some(-1), Some(0)));
        assert!(!empty_integer_gap(Some(i64::MIN), Some(i64::MAX)));
    }

    #[test]
    fn strategy_budget_exhaustion_finishes_instead_of_truncating_inventory() {
        let (_dir, conn) = fixture(false, true);
        index(&conn);
        execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
        seed(&conn, OVERVIEW_PAGE_ROWS * 5, 1, false);
        // These expired-only projects exhaust the discovery strategy first.
        for id in 2..=MAX_INDEX_QUERIES + 2 {
            execute(
                &conn,
                &format!(
                    "INSERT INTO file_reservations VALUES ({}, {id}, 0)",
                    10000 + id
                ),
            );
        }
        execute(
            &conn,
            &format!(
                "INSERT INTO file_reservations VALUES (99999, 999, {})",
                NOW + 1
            ),
        );
        let (_, work) = compare(&conn, NOW);
        assert!(work.reservation_project_rows > 0);
        let rows = build_at_mode(&conn, NOW, true).unwrap().0;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.slug == "[unknown-project-999]"));
    }

    #[test]
    fn counts_snapshot_is_read_only_preserves_outer_transaction_and_cleans_up_errors() {
        let (_dir, conn) = fixture(false, true);
        index(&conn);
        seed(&conn, OVERVIEW_PAGE_ROWS * 5, 77, true);
        execute(&conn, "BEGIN");
        execute(
            &conn,
            &format!(
                "INSERT INTO file_reservations VALUES (99999, 88, {})",
                NOW + 1
            ),
        );
        compare(&conn, NOW);
        assert_eq!(build_at_mode(&conn, NOW, true).unwrap().0.len(), 1);
        execute(&conn, "ROLLBACK");
        execute(&conn, "PRAGMA query_only = ON");
        assert!(build_at_mode(&conn, NOW, true).unwrap().0.is_empty());
        execute(&conn, "PRAGMA query_only = OFF");
        execute(&conn, "DROP TABLE file_reservation_releases");
        execute(
            &conn,
            "CREATE TABLE file_reservation_releases (wrong_column INTEGER)",
        );
        assert!(build_at_mode(&conn, NOW, true).is_err());
        assert!(
            conn.execute_sync("RELEASE robot_overview_read", &[])
                .is_err()
        );
        execute(&conn, "DROP TABLE file_reservation_releases");
        execute(
            &conn,
            "CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY)",
        );
        assert_eq!(build_at_mode(&conn, NOW, true).unwrap().0.len(), 1);
    }

    #[test]
    fn counts_entry_point_does_not_expose_uncounted_reservation_fields() {
        let (_dir, conn) = fixture(false, false);
        execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
        let json: serde_json::Value =
            serde_json::from_str(&build_counts_output(&conn, OutputFormat::Json).unwrap()).unwrap();
        assert_eq!(json["_meta"]["command"], "robot overview");
        assert_eq!(json["project_count"], 1);
        for key in ["unread", "urgent", "ack_overdue"] {
            assert_eq!(json[key], 0);
        }
        assert!(json.get("projects").is_none());
        assert!(json.get("reservations").is_none());
        assert!(
            !build_counts_output(&conn, OutputFormat::Toon)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    #[ignore = "native DbConn reservation-phase benchmark, not end-to-end CLI latency"]
    fn benchmark_counts_reservations_against_full_collection() {
        let (_dir, conn) = fixture(false, true);
        index(&conn);
        seed(&conn, 24000, 1, true);
        execute(
            &conn,
            "UPDATE file_reservations SET project_id = (id % 33) + 1",
        );
        let mut old_times = Vec::new();
        let mut new_times = Vec::new();
        for iteration in 0..6 {
            for counts in [iteration % 2 == 0, iteration % 2 != 0] {
                let mut projects = HashMap::new();
                for id in 1..=33 {
                    project(&mut projects, id);
                }
                let mut work = ScanWork::default();
                let start = Instant::now();
                if counts {
                    collect(&conn, NOW, &mut projects, &mut work).unwrap();
                } else {
                    collect_reservations(&conn, NOW, &mut projects, &mut work).unwrap();
                }
                let elapsed = start.elapsed();
                assert_eq!(projects.len(), 33);
                eprintln!("counts={counts} elapsed={elapsed:?} work={work:?}");
                if counts {
                    new_times.push(elapsed);
                } else {
                    old_times.push(elapsed);
                }
            }
        }
        old_times.sort_unstable();
        new_times.sort_unstable();
        eprintln!(
            "native reservation phase, 33 known projects / 24000 unexpired released rows: full={:?} counts={:?}",
            old_times[3], new_times[3]
        );
    }
}
