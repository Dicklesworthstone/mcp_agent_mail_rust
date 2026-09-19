use super::*;

fn assert_buffers_bounded(work: &ScanWork) {
    assert!(work.peak_query_rows <= OVERVIEW_PAGE_ROWS, "{work:?}");
    assert!(work.peak_message_keys <= OVERVIEW_PAGE_ROWS, "{work:?}");
    assert!(work.peak_release_keys <= OVERVIEW_PAGE_ROWS, "{work:?}");
}

#[test]
fn pending_recipient_fanout_is_bounded_and_keeps_multiplicity() {
    let (_dir, conn) = fixture(false, true);
    let now = ACK_OVERDUE_THRESHOLD_US * 10;
    let recipients = OVERVIEW_PAGE_ROWS * 3 + 7;
    execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
    execute(&conn, "INSERT INTO messages VALUES (1, 1, 'high', 1, 0)");
    execute(&conn, "BEGIN");
    for agent in 1..=recipients {
        // All recipients refer to one message; they must not be deduplicated
        // just because message metadata is fetched once within a batch.
        execute(
            &conn,
            &format!("INSERT INTO message_recipients VALUES (1, {agent}, NULL, NULL)"),
        );
    }
    execute(&conn, "COMMIT");
    let (rows, work) = build_at(&conn, now).expect("bounded fanout");
    assert_eq!(
        (rows[0].unread, rows[0].urgent, rows[0].ack_overdue),
        (recipients, recipients, recipients)
    );
    assert_eq!(work.rows[3], recipients);
    assert_eq!(work.peak_query_rows, OVERVIEW_PAGE_ROWS);
    assert_eq!(work.peak_message_keys, 1);
    assert_eq!(
        work.message_lookup_rows,
        recipients.div_ceil(OVERVIEW_PAGE_ROWS)
    );
    assert_buffers_bounded(&work);
}

#[test]
fn release_lookups_ignore_unrelated_history_and_preserve_null_releases() {
    let (_dir, conn) = fixture(false, true);
    let now = ACK_OVERDUE_THRESHOLD_US * 10;
    execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
    execute(
        &conn,
        "CREATE INDEX idx_expiry ON file_reservations(expires_ts)",
    );
    execute(&conn, "BEGIN");
    for id in 1..=OVERVIEW_PAGE_ROWS * 4 {
        execute(
            &conn,
            &format!("INSERT INTO file_reservations VALUES ({id}, 777, 0, 1)"),
        );
        execute(
            &conn,
            &format!("INSERT INTO file_reservation_releases VALUES ({id}, NULL)"),
        );
    }
    execute(
        &conn,
        &format!(
            "INSERT INTO file_reservations VALUES
        (2000, 1, 0, {}), (2001, 1, 0, {}), (2002, 888, 0, {})",
            now + 1,
            now + 1,
            now + 2
        ),
    );
    execute(
        &conn,
        "INSERT INTO file_reservation_releases VALUES (2002, NULL)",
    );
    execute(&conn, "COMMIT");
    let (rows, work) = build_at(&conn, now).expect("indexed release lookup");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reservations, 2);
    assert_eq!(work.rows[4], 3);
    assert_eq!(work.release_lookup_rows, 1);
    assert_eq!(work.peak_release_keys, 1);
    assert_buffers_bounded(&work);
}

#[test]
fn reservation_expiry_groups_can_span_many_pages_without_loss() {
    let (_dir, conn) = fixture(true, true);
    let now = ACK_OVERDUE_THRESHOLD_US * 10;
    let total = OVERVIEW_PAGE_ROWS * 3 + 17;
    let mut active = 0;
    execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
    execute(
        &conn,
        "CREATE INDEX idx_expiry ON file_reservations(expires_ts)",
    );
    execute(&conn, "BEGIN");
    for id in 1..=total {
        // Non-monotonic ID/expiry relationship, including a very large group
        // of equal expiries. Exercise both cursor phases and their boundaries.
        let expiry = now
            + if id <= OVERVIEW_PAGE_ROWS * 2 {
                1
            } else {
                2 + i64::try_from(id % 3).unwrap()
            };
        let legacy = if id % 7 == 0 { now } else { 0 };
        execute(
            &conn,
            &format!("INSERT INTO file_reservations VALUES ({id}, 1, 0, {expiry}, {legacy})"),
        );
        if id % 5 == 0 {
            execute(
                &conn,
                &format!("INSERT INTO file_reservation_releases VALUES ({id}, NULL)"),
            );
        }
        active += usize::from(id % 7 != 0 && id % 5 != 0);
    }
    execute(&conn, "COMMIT");
    let (rows, work) = build_at(&conn, now).expect("paged expiry groups");
    assert_eq!(rows[0].reservations, active);
    assert_eq!(work.rows[4], total - total / 7);
    assert_buffers_bounded(&work);
}

#[test]
fn cursors_include_negative_minimum_and_maximum_keys() {
    let (_dir, conn) = fixture(false, false);
    let now = ACK_OVERDUE_THRESHOLD_US * 10;
    execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
    execute(
        &conn,
        &format!(
            "INSERT INTO messages VALUES
        ({}, 1, 'high', 1, 0), ({}, 1, 'high', 1, 0)",
            i64::MIN,
            i64::MAX
        ),
    );
    execute(&conn, "BEGIN");
    for index in 0..OVERVIEW_PAGE_ROWS + 1 {
        let index = i64::try_from(index).expect("small fixture index");
        let rowid = if index == 0 {
            i64::MIN
        } else if index == i64::try_from(OVERVIEW_PAGE_ROWS).unwrap() {
            i64::MAX
        } else {
            -index
        };
        let message = if index % 2 == 0 { i64::MIN } else { i64::MAX };
        execute(
            &conn,
            &format!(
                "INSERT INTO message_recipients
            (_rowid_, message_id, agent_id, read_ts, ack_ts)
            VALUES ({rowid}, {message}, {index}, NULL, NULL)"
            ),
        );
    }
    execute(&conn, "COMMIT");
    let (rows, work) = build_at(&conn, now).expect("signed sparse cursors");
    let expected = OVERVIEW_PAGE_ROWS + 1;
    assert_eq!(
        (rows[0].unread, rows[0].urgent, rows[0].ack_overdue),
        (expected, expected, expected)
    );
    assert_eq!(work.rows[2], 2);
    assert_eq!(work.rows[3], expected);
    assert_buffers_bounded(&work);
}

#[test]
fn maximum_expiry_and_exact_page_boundary_terminate() {
    let (_dir, conn) = fixture(false, false);
    execute(&conn, "INSERT INTO projects VALUES (1, 'alpha')");
    execute(&conn, "BEGIN");
    for id in 1..=OVERVIEW_PAGE_ROWS {
        let id = if id == OVERVIEW_PAGE_ROWS {
            i64::MAX
        } else {
            i64::try_from(id).unwrap()
        };
        execute(
            &conn,
            &format!(
                "INSERT INTO file_reservations VALUES ({id}, 1, 0, {})",
                i64::MAX
            ),
        );
    }
    execute(&conn, "COMMIT");
    let (rows, work) = build_at(&conn, i64::MAX - 1).expect("maximum expiry");
    assert_eq!(rows[0].reservations, OVERVIEW_PAGE_ROWS);
    assert_eq!(work.rows[4], OVERVIEW_PAGE_ROWS);
    assert_buffers_bounded(&work);
}

#[test]
fn empty_pending_set_never_fetches_message_metadata_or_release_history() {
    let (_dir, conn) = fixture(false, true);
    seed_scale(&conn, 3, OVERVIEW_PAGE_ROWS);
    execute(
        &conn,
        "UPDATE message_recipients SET read_ts = 0, ack_ts = 0",
    );
    execute(&conn, "INSERT INTO agents VALUES (99999, 777)");
    execute(
        &conn,
        "INSERT INTO messages VALUES (99999, 888, 'normal', 0, 0)",
    );
    let (rows, work) = build_at(&conn, mcp_agent_mail_db::now_micros()).expect("completed inbox");
    assert_eq!(rows.len(), 5);
    assert!(
        rows.iter()
            .all(|row| row.unread == 0 && row.urgent == 0 && row.ack_overdue == 0)
    );
    assert!(rows.iter().any(|row| row.slug == "[unknown-project-777]"));
    assert!(rows.iter().any(|row| row.slug == "[unknown-project-888]"));
    assert_eq!(work.rows[3], 0);
    assert_eq!(work.message_lookup_rows, 0);
    assert_eq!(work.release_lookup_rows, 0);
    assert_buffers_bounded(&work);
}

#[test]
fn lookup_failure_after_multiple_pages_releases_snapshot_and_can_retry() {
    let (_dir, conn) = fixture(false, true);
    seed_scale(&conn, 1, OVERVIEW_PAGE_ROWS + 1);
    execute(
        &conn,
        &format!("UPDATE file_reservations SET expires_ts = {}", i64::MAX),
    );
    execute(&conn, "DROP TABLE file_reservation_releases");
    execute(
        &conn,
        "CREATE TABLE file_reservation_releases (wrong_column INTEGER)",
    );
    assert!(build(&conn).is_err());
    assert!(
        conn.execute_sync("RELEASE robot_overview_read", &[])
            .is_err()
    );
    execute(&conn, "DROP TABLE file_reservation_releases");
    execute(
        &conn,
        "CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY, released_ts INTEGER)",
    );
    let rows = build(&conn).expect("retry after failed indexed lookup");
    assert_eq!(rows[0].reservations, OVERVIEW_PAGE_ROWS + 1);
    assert_eq!(rows[0].unread, OVERVIEW_PAGE_ROWS + 1);
}

#[test]
fn nonprogressing_cursor_and_oversized_lookup_are_rejected() {
    let (_dir, conn) = fixture(false, false);
    execute(&conn, "INSERT INTO agents VALUES (1, 1), (2, 1)");
    let mut work = ScanWork::default();
    let result = scan_pages(
        &conn,
        &mut work,
        Scan {
            select: "SELECT project_id AS id FROM agents",
            predicate: "1 = 1",
            key: "project_id",
            params: &[],
            slot: 1,
        },
        |_, _| panic!("non-unique cursor must not reach the visitor"),
    );
    assert!(result.is_err());
    let ids = vec![1; OVERVIEW_PAGE_ROWS + 1];
    assert!(lookup_ids(&conn, PROJECTS_SQL, "id", &[], &ids, &mut work).is_err());
}
