//! Count unread mail and read-but-unacknowledged mail from disjoint candidates.
//!
//! Most read, non-ack-required mail keeps ack_ts NULL forever. Treating that
//! NULL as actionable materializes historical recipients and repeatedly fetches
//! their messages. The indexed path below never fetches those recipient rows.
//! Its GROUP BY is over one table and at most one page of message IDs, not a
//! mailbox-wide recipient/message join. Older read-only schemas keep the
//! original bounded scanner instead of creating indexes during an observation.

use super::*;

const UNREAD_FILTER: &str = "read_ts IS NULL";
const ACK_MESSAGES_SQL: &str = "SELECT id, project_id FROM messages";
const ACK_MESSAGES_FILTER: &str = "ack_required = 1 AND created_ts < ?";
const ACK_COUNTS_SQL: &str = "SELECT message_id, COUNT(*) AS pending_count FROM message_recipients";
const ACK_PENDING_IDS_SQL: &str = "SELECT message_id FROM message_recipients";
const ACK_PENDING_FILTER: &str = "ack_ts IS NULL AND message_id IS NOT NULL";

/// Return false without modifying project counters when indexes are absent or
/// a bounded sample is unread-heavy. Work includes the strategy sample, which
/// the fallback intentionally rereads; this costs at most one page, not history.
/// All probes and both counting passes share build_at's read snapshot.
pub(super) fn try_collect(
    conn: &DbConn,
    now_us: i64,
    projects: &mut HashMap<i64, OverviewProject>,
    work: &mut ScanWork,
) -> Result<bool, CliError> {
    let Some(message_index) = full_index_with_prefix(conn, "messages", &["ack_required", "id"])?
    else {
        return Ok(false);
    };
    let Some(recipient_index) =
        full_index_with_prefix(conn, "message_recipients", &["ack_ts", "message_id"])?
    else {
        return Ok(false);
    };
    // Prefer the existing one-pass collector for unread-heavy traffic: it
    // already gets overdue metadata with the unread lookup. Do not decide by
    // counting a whole table, or let an all-unread mailbox pay another full
    // ack-index scan merely to discover that no read-but-unacked rows exist.
    let sample = bounded_query(
        conn,
        &format!(
            "{RECIPIENTS_SQL} WHERE ({RECIPIENT_FILTER}) ORDER BY _rowid_ LIMIT {OVERVIEW_PAGE_ROWS}",
        ),
        &[],
        work,
    )?;
    work.recipient_sample_rows += sample.len();
    if sample.is_empty() {
        return Ok(true);
    }
    let mut read = 0;
    for row in &sample {
        read += usize::from(integer(row, "unread")? == 0);
    }
    if read * 2 < sample.len() {
        return Ok(false);
    }
    drop(sample);
    let threshold = micros_ago(now_us, ACK_OVERDUE_THRESHOLD_US);
    scan_pages(
        conn,
        work,
        Scan {
            select: RECIPIENTS_SQL,
            predicate: UNREAD_FILTER,
            key: "_rowid_",
            params: &[],
            slot: 3,
        },
        |rows, work| {
            let mut ids: Vec<_> = rows
                .iter()
                .filter_map(|row| row.get_by_name("message_id").and_then(Value::as_i64))
                .collect();
            ids.sort_unstable();
            ids.dedup();
            let metadata = lookup_ids(
                conn,
                MESSAGES_SQL,
                "id",
                &[Value::BigInt(threshold)],
                &ids,
                work,
            )?;
            work.message_lookup_rows += metadata.len();
            let mut messages = HashMap::with_capacity(metadata.len());
            for row in metadata {
                messages.insert(
                    integer(&row, "id")?,
                    (
                        integer(&row, "project_id")?,
                        integer(&row, "urgent")? != 0,
                        integer(&row, "overdue")? != 0,
                    ),
                );
            }
            work.peak_message_keys = work.peak_message_keys.max(messages.len());
            for row in rows {
                // Preserve INNER JOIN semantics for dangling recipients and count
                // each recipient, not each distinct message in this metadata page.
                let Some(&(project_id, urgent, overdue)) = row
                    .get_by_name("message_id")
                    .and_then(Value::as_i64)
                    .and_then(|id| messages.get(&id))
                else {
                    continue;
                };
                let counts = project(projects, project_id);
                counts.unread += 1;
                counts.urgent += usize::from(urgent);
                if overdue && integer(row, "unacked")? != 0 {
                    counts.ack_overdue += 1;
                }
            }
            Ok(())
        },
    )?;

    collect_read_ack_counts(
        conn,
        threshold,
        &message_index,
        &recipient_index,
        projects,
        work,
    )?;
    Ok(true)
}

/// Seek a covering (ack_ts, message_id) index rather than walking settled mail.
/// Do NOT filter read_ts here: that would require visiting recipient payloads
/// and could scan arbitrarily large unread fanout just to find the next key.
/// Unread, non-ack-required and dangling recipients are safe false positives;
/// the normal message predicate and grouped count still decide what counts.
fn next_unacked_id(
    conn: &DbConn,
    recipient_index: &str,
    after: Option<i64>,
    work: &mut ScanWork,
) -> Result<Option<i64>, CliError> {
    let (continuation, params) = after.map_or_else(
        || ("", Vec::new()),
        |id| (" AND message_id > ?", vec![Value::BigInt(id)]),
    );
    let sql = format!(
        "{ACK_PENDING_IDS_SQL} INDEXED BY {} WHERE {ACK_PENDING_FILTER}\
         {continuation} ORDER BY message_id LIMIT 1",
        quoted_identifier(recipient_index),
    );
    let rows = bounded_query(conn, &sql, &params, work)?;
    if rows.len() > 1 {
        return Err(CliError::Other(
            "overview acknowledgement seek exceeded one row".to_string(),
        ));
    }
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let id = integer(row, "message_id")?;
    if after.is_some_and(|previous| id <= previous) {
        return Err(CliError::Other(
            "overview acknowledgement seek did not advance".to_string(),
        ));
    }
    Ok(Some(id))
}

/// Skip settled prefixes, suffixes and gaps without sampling away obligations.
/// The caller verified both indexes and holds one read snapshot for all phases.
/// Seek initially and once per streak of full pages with no read-ack groups.
/// Repeated non-ack decoys must not add a query to every historical page;
/// another nonempty count page re-arms the lookahead. Dense pending pages keep
/// the previous two-query batching plan. No seek uses id + 1, so i64::MIN/MAX
/// and an obligation exactly at the seek key are safe.
fn collect_read_ack_counts(
    conn: &DbConn,
    threshold: i64,
    message_index: &str,
    recipient_index: &str,
    projects: &mut HashMap<i64, OverviewProject>,
    work: &mut ScanWork,
) -> Result<(), CliError> {
    let Some(mut lower) = next_unacked_id(conn, recipient_index, None, work)? else {
        return Ok(());
    };
    let mut inclusive = true;
    let mut seek_empty = true;
    loop {
        let comparison = if inclusive { ">=" } else { ">" };
        let sql = format!(
            "{ACK_MESSAGES_SQL} INDEXED BY {} WHERE ({ACK_MESSAGES_FILTER}) \
             AND id {comparison} ? ORDER BY id LIMIT {OVERVIEW_PAGE_ROWS}",
            quoted_identifier(message_index),
        );
        let rows = bounded_query(
            conn,
            &sql,
            &[Value::BigInt(threshold), Value::BigInt(lower)],
            work,
        )?;
        work.ack_message_rows += rows.len();
        let mut last = None;
        for row in &rows {
            let id = integer(row, "id")?;
            if id < lower
                || (!inclusive && id == lower)
                || last.is_some_and(|previous| id <= previous)
            {
                return Err(CliError::Other(
                    "overview acknowledgement message cursor did not advance".to_string(),
                ));
            }
            last = Some(id);
        }
        let Some(last) = last else {
            return Ok(());
        };
        let had_counts = count_read_ack_page(conn, recipient_index, &rows, projects, work)?;
        if rows.len() < OVERVIEW_PAGE_ROWS {
            return Ok(());
        }
        if !had_counts && seek_empty {
            seek_empty = false;
            // An empty result alone proves nothing about later messages.
            // Advance only as far as the exact next unacknowledged index key.
            let Some(next) = next_unacked_id(conn, recipient_index, Some(last), work)? else {
                return Ok(());
            };
            lower = next;
            inclusive = true;
        } else {
            lower = last;
            inclusive = false;
            if had_counts {
                seek_empty = true;
            }
        }
    }
}

fn count_read_ack_page(
    conn: &DbConn,
    recipient_index: &str,
    rows: &[Row],
    projects: &mut HashMap<i64, OverviewProject>,
    work: &mut ScanWork,
) -> Result<bool, CliError> {
    let mut message_projects = HashMap::with_capacity(rows.len());
    let mut params = Vec::with_capacity(rows.len());
    for row in rows {
        let id = integer(row, "id")?;
        message_projects.insert(id, integer(row, "project_id")?);
        params.push(Value::BigInt(id));
    }
    work.peak_message_keys = work.peak_message_keys.max(message_projects.len());
    let placeholders = vec!["?"; params.len()].join(",");
    // Same grouped lookup as before the seek optimization: the index key
    // does not decide visibility or multiplicity. Exclude unread recipients
    // already counted in the first pass. Do not LIMIT away unexpected groups.
    let sql = format!(
        "{ACK_COUNTS_SQL} INDEXED BY {} \
         WHERE ack_ts IS NULL AND read_ts IS NOT NULL \
         AND message_id IN ({placeholders}) GROUP BY message_id",
        quoted_identifier(recipient_index),
    );
    let counts = bounded_query(conn, &sql, &params, work)?;
    let had_counts = !counts.is_empty();
    work.ack_count_rows += counts.len();
    for row in counts {
        let id = integer(&row, "message_id")?;
        let project_id = message_projects.remove(&id).ok_or_else(|| {
            CliError::Other(
                "overview acknowledgement group was duplicate or unrequested".to_string(),
            )
        })?;
        let count = usize::try_from(integer(&row, "pending_count")?).map_err(|_| {
            CliError::Other(
                "overview acknowledgement count was negative or overflowed".to_string(),
            )
        })?;
        let counts = project(projects, project_id);
        counts.ack_overdue = counts.ack_overdue.checked_add(count).ok_or_else(|| {
            CliError::Other("overview acknowledgement total overflowed".to_string())
        })?;
    }
    Ok(had_counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const NOW: i64 = 18_000_000_000;

    fn execute(conn: &DbConn, sql: &str) {
        conn.execute_sync(sql, &[]).expect(sql);
    }

    fn fixture() -> (tempfile::TempDir, DbConn) {
        let dir = tempfile::tempdir().expect("recipient fixture directory");
        let conn = DbConn::open_file(dir.path().join("recipients.sqlite3").to_str().unwrap())
            .expect("native recipient fixture");
        for sql in [
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL)",
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL)",
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, importance TEXT, ack_required INTEGER, created_ts INTEGER)",
            "CREATE INDEX inventory ON messages(project_id)",
            "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id))",
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, expires_ts INTEGER)",
            "INSERT INTO projects VALUES (1, 'alpha'), (2, 'empty')",
        ] {
            execute(&conn, sql);
        }
        (dir, conn)
    }

    fn indexes(conn: &DbConn) {
        execute(
            conn,
            "CREATE INDEX ack_messages ON messages(ack_required, id)",
        );
        execute(
            conn,
            "CREATE INDEX ack_recipients ON message_recipients(ack_ts, message_id)",
        );
    }

    fn json(rows: &[OverviewProject]) -> serde_json::Value {
        serde_json::to_value(rows).expect("serialize overview")
    }

    fn compare_before_and_after_indexes(conn: &DbConn) -> (ScanWork, ScanWork) {
        let (expected, old) = build_at(conn, NOW).expect("original scanner");
        indexes(conn);
        let (actual, new) = build_at(conn, NOW).expect("indexed counters");
        assert_eq!(json(&actual), json(&expected));
        assert!(new.peak_query_rows <= OVERVIEW_PAGE_ROWS);
        assert!(new.peak_message_keys <= OVERVIEW_PAGE_ROWS);
        (old, new)
    }

    fn seed_history(conn: &DbConn, size: usize) {
        execute(conn, "BEGIN");
        for start in (1..=size).step_by(OVERVIEW_PAGE_ROWS) {
            let end = (start + OVERVIEW_PAGE_ROWS - 1).min(size);
            let messages: Vec<_> = (start..=end)
                .map(|id| format!("({id}, 1, 'normal', 0, 0)"))
                .collect();
            let recipients: Vec<_> = (start..=end)
                .map(|id| format!("({id}, 1, 0, NULL)"))
                .collect();
            execute(
                conn,
                &format!("INSERT INTO messages VALUES {}", messages.join(",")),
            );
            execute(
                conn,
                &format!(
                    "INSERT INTO message_recipients VALUES {}",
                    recipients.join(",")
                ),
            );
        }
        execute(conn, "COMMIT");
    }

    #[test]
    fn read_non_ack_history_does_not_materialize_recipients_or_message_lookups() {
        for history in [OVERVIEW_PAGE_ROWS * 4 + 1, OVERVIEW_PAGE_ROWS * 16 + 1] {
            let (_dir, conn) = fixture();
            seed_history(&conn, history);
            execute(
                &conn,
                "INSERT INTO messages VALUES (99999, 77, 'high', 1, 0)",
            );
            execute(
                &conn,
                "INSERT INTO message_recipients VALUES (99999, 1, NULL, NULL), (99999, 2, 0, NULL)",
            );
            let (old, new) = compare_before_and_after_indexes(&conn);
            assert_eq!(old.rows[3], history + 2);
            assert_eq!(new.rows[3], 1);
            assert_eq!(new.message_lookup_rows, 1);
            assert_eq!(new.ack_message_rows, 1);
            assert_eq!(new.ack_count_rows, 1);
            assert!(new.queries < old.queries, "old={old:?} new={new:?}");
            let rows = build_at(&conn, NOW).unwrap().0;
            let orphan = rows
                .iter()
                .find(|row| row.slug == "[unknown-project-77]")
                .unwrap();
            assert_eq!(
                (orphan.unread, orphan.urgent, orphan.ack_overdue),
                (1, 1, 2)
            );
        }
    }

    #[test]
    fn read_acknowledgement_fanout_is_one_group_not_unbounded_recipient_rows() {
        let (_dir, conn) = fixture();
        let fanout = OVERVIEW_PAGE_ROWS * 3 + 7;
        execute(&conn, "INSERT INTO messages VALUES (1, 1, 'high', 1, 0)");
        execute(&conn, "BEGIN");
        for agent in 1..=fanout {
            execute(
                &conn,
                &format!("INSERT INTO message_recipients VALUES (1, {agent}, 0, NULL)"),
            );
        }
        execute(&conn, "COMMIT");
        let (_, new) = compare_before_and_after_indexes(&conn);
        assert_eq!(new.rows[3], 0);
        assert_eq!(new.message_lookup_rows, 0);
        assert_eq!(new.ack_message_rows, 1);
        assert_eq!(new.ack_count_rows, 1);
        let rows = build_at(&conn, NOW).unwrap().0;
        assert_eq!(rows[0].ack_overdue, fanout);
    }

    #[test]
    fn strict_deadlines_nulls_case_and_dangling_recipients_match_original() {
        let (_dir, conn) = fixture();
        let threshold = micros_ago(NOW, ACK_OVERDUE_THRESHOLD_US);
        execute(
            &conn,
            &format!(
                "INSERT INTO messages VALUES
            (1, 1, 'high', 1, {}), (2, 1, 'urgent', 1, {threshold}),
            (3, 1, 'normal', 1, {}), (4, 1, NULL, NULL, NULL),
            (5, 1, 'URGENT', 0, 0), (6, 88, 'high', 0, 0)",
                threshold - 1,
                threshold + 1
            ),
        );
        execute(
            &conn,
            "INSERT INTO message_recipients VALUES
            (1, 1, NULL, NULL), (1, 2, 0, NULL), (1, 3, NULL, 0),
            (1, 4, 0, 0), (2, 1, NULL, NULL), (3, 1, NULL, NULL),
            (4, 1, NULL, NULL), (5, 1, NULL, NULL), (999, 1, NULL, NULL),
            (5, 2, 0, NULL), (5, 3, 0, NULL), (5, 4, 0, NULL),
            (5, 5, 0, NULL), (5, 6, 0, NULL), (5, 7, 0, NULL)",
        );
        let (_, work) = compare_before_and_after_indexes(&conn);
        assert_eq!(work.ack_message_rows, 1);
        let rows = build_at(&conn, NOW).unwrap().0;
        let alpha = rows.iter().find(|row| row.slug == "alpha").unwrap();
        assert_eq!((alpha.unread, alpha.urgent, alpha.ack_overdue), (6, 3, 2));
        assert!(rows.iter().any(|row| row.slug == "[unknown-project-88]"));
        assert!(!rows.iter().any(|row| row.slug == "[unknown-project-999]"));
    }

    #[test]
    fn acknowledgement_message_pages_include_signed_extremes_and_exact_boundaries() {
        for size in [OVERVIEW_PAGE_ROWS, OVERVIEW_PAGE_ROWS * 2 + 1] {
            let (_dir, conn) = fixture();
            execute(&conn, "BEGIN");
            for ordinal in 0..size {
                let id = if ordinal == 0 {
                    i64::MIN
                } else if ordinal == size - 1 {
                    i64::MAX
                } else {
                    -i64::try_from(ordinal).unwrap()
                };
                execute(
                    &conn,
                    &format!("INSERT INTO messages VALUES ({id}, 1, 'normal', 1, 0)"),
                );
                execute(
                    &conn,
                    &format!(
                        "INSERT INTO message_recipients VALUES ({id}, 1, 0, NULL), ({id}, 2, 0, 0)"
                    ),
                );
            }
            execute(&conn, "COMMIT");
            let (_, new) = compare_before_and_after_indexes(&conn);
            assert_eq!(new.ack_message_rows, size);
            assert_eq!(new.ack_count_rows, size);
            assert_eq!(build_at(&conn, NOW).unwrap().0[0].ack_overdue, size);
        }
    }

    #[test]
    fn fully_acknowledged_history_skips_the_ack_message_scan() {
        let (_dir, conn) = fixture();
        seed_history(&conn, OVERVIEW_PAGE_ROWS * 3);
        execute(&conn, "UPDATE messages SET ack_required = 1");
        execute(&conn, "UPDATE message_recipients SET ack_ts = 0");
        let (_, new) = compare_before_and_after_indexes(&conn);
        assert_eq!(new.rows[3], 0);
        assert_eq!(new.ack_message_rows, 0);
        assert_eq!(new.ack_count_rows, 0);
        assert!(
            build_at(&conn, NOW)
                .unwrap()
                .0
                .iter()
                .all(|row| row.ack_overdue == 0)
        );
    }

    #[test]
    fn all_unread_mail_is_counted_once_without_a_second_ack_message_walk() {
        let (_dir, conn) = fixture();
        let size = OVERVIEW_PAGE_ROWS * 2 + 3;
        seed_history(&conn, size);
        execute(&conn, "UPDATE messages SET ack_required = 1");
        execute(&conn, "UPDATE message_recipients SET read_ts = NULL");
        let (_, new) = compare_before_and_after_indexes(&conn);
        assert_eq!(new.rows[3], size);
        assert_eq!(new.ack_message_rows, 0);
        assert_eq!(new.ack_count_rows, 0);
        let rows = build_at(&conn, NOW).unwrap().0;
        assert_eq!((rows[0].unread, rows[0].ack_overdue), (size, size));
    }

    #[test]
    fn unsupported_index_shapes_leave_counters_untouched_and_keep_the_fallback() {
        for ddl in [
            "CREATE INDEX unsuitable ON messages(ack_required) WHERE ack_required = 1",
            "CREATE INDEX unsuitable ON messages(id, ack_required)",
            "CREATE INDEX unsuitable ON messages((ack_required + 0), id)",
            "CREATE INDEX unsuitable ON messages(ack_required, created_ts, id)",
        ] {
            let (_dir, conn) = fixture();
            execute(&conn, ddl);
            execute(
                &conn,
                "CREATE INDEX ack_recipients ON message_recipients(ack_ts,message_id)",
            );
            seed_history(&conn, 1);
            let mut projects = HashMap::new();
            let mut work = ScanWork::default();
            assert!(!try_collect(&conn, NOW, &mut projects, &mut work).unwrap());
            assert!(projects.is_empty());
            assert_eq!(work, ScanWork::default());
            assert_eq!(build_at(&conn, NOW).unwrap().1.rows[3], 1);
        }
        let (_dir, conn) = fixture();
        execute(
            &conn,
            "CREATE INDEX ack_messages ON messages(ack_required, id)",
        );
        execute(
            &conn,
            "CREATE INDEX wrong_order ON message_recipients(message_id, ack_ts)",
        );
        let mut projects = HashMap::new();
        let mut work = ScanWork::default();
        assert!(!try_collect(&conn, NOW, &mut projects, &mut work).unwrap());
        assert_eq!(work, ScanWork::default());
    }

    #[test]
    fn quoted_indexes_work_inside_a_read_only_caller_transaction() {
        let (_dir, conn) = fixture();
        execute(
            &conn,
            "CREATE INDEX \"ack\"\"messages\" ON messages(ack_required, id)",
        );
        execute(
            &conn,
            "CREATE INDEX \"ack\"\"recipients\" ON message_recipients(ack_ts,message_id)",
        );
        execute(&conn, "INSERT INTO messages VALUES (1, 1, 'urgent', 1, 0)");
        execute(
            &conn,
            "INSERT INTO message_recipients VALUES (1, 1, 0, NULL)",
        );
        execute(&conn, "PRAGMA query_only = ON");
        execute(&conn, "BEGIN");
        let (rows, work) = build_at(&conn, NOW).expect("query-only indexed overview");
        assert_eq!(rows[0].ack_overdue, 1);
        assert_eq!(work.ack_count_rows, 1);
        execute(&conn, "ROLLBACK");
    }

    #[test]
    fn indexed_counts_recompute_time_and_mutations_without_a_max_timestamp_shortcut() {
        let (_dir, conn) = fixture();
        indexes(&conn);
        let threshold = micros_ago(NOW, ACK_OVERDUE_THRESHOLD_US);
        execute(
            &conn,
            &format!(
                "INSERT INTO messages VALUES (1, 1, 'high', 1, {threshold}), (2, 1, 'normal', 0, 0)"
            ),
        );
        execute(
            &conn,
            "INSERT INTO message_recipients VALUES (1, 1, NULL, NULL), (2, 1, 999, 999), (2, 2, 0, NULL)",
        );
        let before = build_at(&conn, NOW).unwrap().0;
        let overdue = build_at(&conn, NOW + 1).unwrap().0;
        assert_eq!((before[0].unread, before[0].ack_overdue), (1, 0));
        assert_eq!((overdue[0].unread, overdue[0].ack_overdue), (1, 1));
        execute(
            &conn,
            "UPDATE message_recipients SET read_ts = 1, ack_ts = 1 WHERE message_id = 1",
        );
        let after = build_at(&conn, NOW + 1).unwrap().0;
        assert_eq!(
            (after[0].unread, after[0].urgent, after[0].ack_overdue),
            (0, 0, 0)
        );
    }

    // The incumbent acknowledgement page walk from 38c9d8a8. Keep its real
    // native query plan beside the new path, not a wall-clock estimate or a
    // source-text assertion. The grouped count helper is unchanged.
    fn ack_phase(conn: &DbConn, seek: bool) -> (serde_json::Value, ScanWork) {
        let mut projects = HashMap::new();
        let mut work = ScanWork::default();
        let threshold = micros_ago(NOW, ACK_OVERDUE_THRESHOLD_US);
        if seek {
            collect_read_ack_counts(
                conn,
                threshold,
                "ack_messages",
                "ack_recipients",
                &mut projects,
                &mut work,
            )
            .expect("seek acknowledgement phase");
        } else {
            scan_pages(
                conn,
                &mut work,
                Scan {
                    select: "SELECT id, project_id FROM messages INDEXED BY ack_messages",
                    predicate: ACK_MESSAGES_FILTER,
                    key: "id",
                    params: &[Value::BigInt(threshold)],
                    slot: 2,
                },
                |rows, work| {
                    count_read_ack_page(conn, "ack_recipients", rows, &mut projects, work)?;
                    Ok(())
                },
            )
            .expect("incumbent acknowledgement phase");
            work.ack_message_rows = work.rows[2];
        }
        (serde_json::to_value(projects).unwrap(), work)
    }

    fn seed_settled_ack_history(conn: &DbConn, size: usize) {
        seed_history(conn, size);
        execute(conn, "UPDATE messages SET ack_required = 1");
        execute(conn, "UPDATE message_recipients SET ack_ts = 0");
    }

    #[test]
    fn settled_ack_prefix_is_skipped_even_with_pending_non_ack_mail() {
        for history in [OVERVIEW_PAGE_ROWS * 4 + 1, OVERVIEW_PAGE_ROWS * 16 + 1] {
            let (_dir, conn) = fixture();
            seed_settled_ack_history(&conn, history);
            execute(&conn, "INSERT INTO messages VALUES (99999, 77, 'high', 1, 0)");
            execute(
                &conn,
                "INSERT INTO message_recipients VALUES (99999, 1, 0, NULL), (99999, 2, NULL, NULL)",
            );
            // Fallback vs optimized *full* overview also covers the unread
            // contribution. The sought ID is inclusive and its fanout is exact.
            let (_, new) = compare_before_and_after_indexes(&conn);
            assert_eq!(new.ack_message_rows, 1);
            let (expected, old) = ack_phase(&conn, false);
            let (actual, work) = ack_phase(&conn, true);
            assert_eq!(actual, expected);
            assert_eq!(actual["77"]["ack_overdue"], 1);
            assert_eq!(old.ack_message_rows, history + 1);
            assert_eq!(work.ack_message_rows, 1);
            assert_eq!(work.queries, 3);
            assert!(work.queries < old.queries);
            execute(&conn, "UPDATE messages SET ack_required = 0 WHERE id = 99999");
            let (actual, work) = ack_phase(&conn, true);
            assert_eq!(actual, ack_phase(&conn, false).0);
            assert_eq!(work.ack_message_rows, 0);
            assert_eq!(work.queries, 2);
        }
    }

    #[test]
    fn non_ack_decoy_does_not_force_a_complete_settled_gap_walk() {
        let (_dir, conn) = fixture();
        seed_settled_ack_history(&conn, OVERVIEW_PAGE_ROWS * 16 + 1);
        execute(
            &conn,
            "INSERT INTO messages VALUES (0, 1, 'normal', 0, 0), (99999, 77, 'high', 1, 0)",
        );
        execute(
            &conn,
            "INSERT INTO message_recipients VALUES (0, 1, 0, NULL), (99999, 1, 0, NULL)",
        );
        let (_, work) = compare_before_and_after_indexes(&conn);
        assert_eq!(work.ack_message_rows, OVERVIEW_PAGE_ROWS + 1);
        let (actual, work) = ack_phase(&conn, true);
        assert_eq!(actual, ack_phase(&conn, false).0);
        assert_eq!(actual["77"]["ack_overdue"], 1);
        assert_eq!(work.queries, 6);
        assert!(work.peak_query_rows <= OVERVIEW_PAGE_ROWS);
    }

    #[test]
    fn dense_read_ack_pages_pay_one_initial_seek_not_one_per_page() {
        let (_dir, conn) = fixture();
        let size = OVERVIEW_PAGE_ROWS * 3 + 7;
        seed_history(&conn, size);
        execute(&conn, "UPDATE messages SET ack_required = 1");
        indexes(&conn);
        let (expected, old) = ack_phase(&conn, false);
        let (actual, work) = ack_phase(&conn, true);
        assert_eq!(actual, expected);
        assert_eq!(actual["1"]["ack_overdue"], size);
        assert_eq!(work.ack_message_rows, size);
        assert_eq!(work.queries, old.queries + 1);
    }

    #[test]
    fn interleaved_non_ack_decoys_do_not_add_a_seek_to_every_empty_page() {
        let (_dir, conn) = fixture();
        seed_history(&conn, OVERVIEW_PAGE_ROWS * 8 + 2);
        execute(&conn, "UPDATE messages SET ack_required = id % 2");
        execute(&conn, "UPDATE message_recipients SET ack_ts = CASE WHEN message_id % 2 = 1 THEN 0 ELSE NULL END");
        indexes(&conn);
        let (expected, old) = ack_phase(&conn, false);
        let (actual, work) = ack_phase(&conn, true);
        assert_eq!(actual, expected);
        assert_eq!(actual, serde_json::json!({}));
        // Skipping the first settled key can also remove a partial page.
        assert!(work.queries <= old.queries + 2);
    }

    #[test]
    fn seek_keeps_signed_endpoints_and_unread_dangling_false_positives() {
        let (_dir, conn) = fixture();
        indexes(&conn);
        execute(
            &conn,
            &format!(
                "INSERT INTO messages VALUES ({}, 1, 'high', 1, 0), ({}, 77, 'normal', 1, 0)",
                i64::MIN,
                i64::MAX
            ),
        );
        execute(
            &conn,
            &format!(
                "INSERT INTO message_recipients VALUES ({}, 1, 0, NULL), \
                 (-1, 1, NULL, NULL), (0, 1, 0, 0), ({}, 1, 0, NULL)",
                i64::MIN,
                i64::MAX
            ),
        );
        let mut work = ScanWork::default();
        assert_eq!(next_unacked_id(&conn, "ack_recipients", None, &mut work).unwrap(), Some(i64::MIN));
        assert_eq!(next_unacked_id(&conn, "ack_recipients", Some(i64::MIN), &mut work).unwrap(), Some(-1));
        assert_eq!(next_unacked_id(&conn, "ack_recipients", Some(-1), &mut work).unwrap(), Some(i64::MAX));
        assert_eq!(next_unacked_id(&conn, "ack_recipients", Some(i64::MAX), &mut work).unwrap(), None);
        assert_eq!(ack_phase(&conn, true).0, ack_phase(&conn, false).0);
        assert!(next_unacked_id(&conn, "missing_index", None, &mut work).is_err());
    }

    #[test]
    fn earlier_reopened_obligation_is_visible_on_the_next_overview() {
        let (_dir, conn) = fixture();
        seed_settled_ack_history(&conn, OVERVIEW_PAGE_ROWS * 4);
        execute(&conn, "INSERT INTO messages VALUES (99999, 77, 'normal', 0, 0)");
        execute(&conn, "INSERT INTO message_recipients VALUES (99999, 1, 0, NULL)");
        indexes(&conn);
        assert!(build_at(&conn, NOW).unwrap().0.iter().all(|row| row.ack_overdue == 0));
        execute(&conn, "UPDATE message_recipients SET ack_ts = NULL WHERE message_id = 1");
        assert_eq!(build_at(&conn, NOW).unwrap().0.iter().find(|row| row.slug == "alpha").unwrap().ack_overdue, 1);
        execute(&conn, "UPDATE message_recipients SET ack_ts = 0 WHERE message_id = 1");
        assert!(build_at(&conn, NOW).unwrap().0.iter().all(|row| row.ack_overdue == 0));
    }

    #[test]
    fn seek_and_count_share_the_native_wal_read_snapshot() {
        let (dir, conn) = fixture();
        indexes(&conn);
        execute(&conn, "PRAGMA journal_mode = WAL");
        execute(&conn, "INSERT INTO messages VALUES (1, 1, 'high', 1, 0), (2, 77, 'high', 1, 0)");
        execute(&conn, "INSERT INTO message_recipients VALUES (1, 1, 0, NULL), (2, 1, 0, 0)");
        let writer = DbConn::open_file(dir.path().join("recipients.sqlite3").to_str().unwrap()).unwrap();
        execute(&conn, "SAVEPOINT held_overview");
        let mut work = ScanWork::default();
        assert_eq!(next_unacked_id(&conn, "ack_recipients", None, &mut work).unwrap(), Some(1));
        execute(&writer, "BEGIN IMMEDIATE");
        execute(&writer, "UPDATE message_recipients SET ack_ts = 1 WHERE message_id = 1");
        execute(&writer, "UPDATE message_recipients SET ack_ts = NULL WHERE message_id = 2");
        execute(&writer, "COMMIT");
        let held = ack_phase(&conn, true).0;
        assert_eq!(held["1"]["ack_overdue"], 1);
        assert!(held.get("77").is_none());
        execute(&conn, "RELEASE held_overview");
        let fresh = ack_phase(&conn, true).0;
        assert!(fresh.get("1").is_none());
        assert_eq!(fresh["77"]["ack_overdue"], 1);
    }

    #[test]
    #[ignore = "native acknowledgement-phase comparison, including dense adverse case"]
    fn benchmark_ack_seek_against_current_page_walk() {
        let history = std::env::var("AM_OVERVIEW_ACK_HISTORY").map_or(24_000, |value| {
            value.parse().expect("positive history size")
        });
        assert!(history > 0);
        for settled in [true, false] {
            let (_dir, conn) = fixture();
            seed_settled_ack_history(&conn, history);
            execute(&conn, "UPDATE messages SET project_id = id % 33 + 1");
            execute(&conn, "INSERT INTO messages VALUES (0, 1, 'normal', 0, 0)");
            execute(&conn, "INSERT INTO message_recipients VALUES (0, 1, 0, NULL)");
            let active = history + 1;
            execute(&conn, &format!("INSERT INTO messages VALUES ({active}, 1, 'high', 1, 0)"));
            execute(&conn, &format!("INSERT INTO message_recipients VALUES ({active}, 1, 0, NULL)"));
            if !settled {
                execute(&conn, "UPDATE message_recipients SET ack_ts = NULL");
            }
            indexes(&conn);
            let expected = ack_phase(&conn, false).0;
            let mut old_times = Vec::new();
            let mut new_times = Vec::new();
            for iteration in 0..6 {
                for seek in [iteration % 2 == 0, iteration % 2 != 0] {
                    let start = Instant::now();
                    let (actual, work) = ack_phase(&conn, seek);
                    let elapsed = start.elapsed();
                    assert_eq!(actual, expected);
                    eprintln!("settled={settled} seek={seek} elapsed={elapsed:?} work={work:?}");
                    if seek {
                        new_times.push(elapsed);
                    } else {
                        old_times.push(elapsed);
                    }
                }
            }
            old_times.sort_unstable();
            new_times.sort_unstable();
            eprintln!("native ack phase; history={history}; settled={settled}; old={:?}; new={:?}", old_times[3], new_times[3]);
        }
    }

    #[test]
    #[ignore = "native DbConn recipient-phase benchmark; not end-to-end CLI latency"]
    fn benchmark_sparse_recipient_counts_against_pending_row_scan() {
        let (_dir, conn) = fixture();
        let history = std::env::var("AM_OVERVIEW_RECIPIENT_HISTORY").map_or(24_000, |value| {
            value.parse().expect("positive history size")
        });
        assert!(history > 0);
        seed_history(&conn, history);
        let active = history + 1;
        execute(
            &conn,
            &format!("INSERT INTO messages VALUES ({active}, 1, 'high', 1, 0)"),
        );
        execute(
            &conn,
            &format!(
                "INSERT INTO message_recipients VALUES ({active}, 1, NULL, NULL), ({active}, 2, 0, NULL)"
            ),
        );
        indexes(&conn);
        let mut baseline = None;
        let mut old_times = Vec::new();
        let mut new_times = Vec::new();
        for iteration in 0..6 {
            for sparse in [iteration % 2 == 0, iteration % 2 != 0] {
                let mut projects = HashMap::new();
                let mut work = ScanWork::default();
                let start = Instant::now();
                if sparse {
                    assert!(try_collect(&conn, NOW, &mut projects, &mut work).unwrap());
                } else {
                    collect_recipients_scan(&conn, NOW, &mut projects, &mut work).unwrap();
                }
                let elapsed = start.elapsed();
                let value = serde_json::to_value(&projects).unwrap();
                if let Some(expected) = &baseline {
                    assert_eq!(&value, expected);
                } else {
                    baseline = Some(value);
                }
                eprintln!("sparse={sparse} elapsed={elapsed:?} work={work:?}");
                if sparse {
                    new_times.push(elapsed);
                } else {
                    old_times.push(elapsed);
                }
            }
        }
        old_times.sort_unstable();
        new_times.sort_unstable();
        eprintln!(
            "native recipient phase only; read_non_ack_history={history}; old_median={:?}; new_median={:?}",
            old_times[3], new_times[3]
        );
    }
}
