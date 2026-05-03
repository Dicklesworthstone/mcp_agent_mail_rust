#![allow(clippy::too_many_lines)]

use mcp_agent_mail_db::schema::CREATE_TABLES_SQL;
use mcp_agent_mail_db::{
    DbConn, TimestampFormat, convert_all_timestamps, convert_column, detect_timestamp_format,
    micros_to_iso, text_to_micros,
};

const TS_WITH_MICROS: i64 = 1_771_947_000_123_456;
const TS_NO_FRACTION: i64 = 1_771_947_000_000_000;
const TS_DATE_ONLY: i64 = 1_771_891_200_000_000;

fn parse_vector(input: &str) -> Option<i64> {
    text_to_micros(input, "vectors", "ts", 1).expect("timestamp parse should succeed")
}

fn seed_full_migration_fixture(conn: &DbConn) {
    conn.query_sync(
        "INSERT INTO projects (slug, human_key, created_at)
         VALUES ('proj', '/tmp/proj', '2026-02-24 15:30:00.123456')",
        &[],
    )
    .expect("insert project");
    conn.query_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts)
         VALUES (1, 'BlueLake', 'cc', 'opus', '2026-02-24T15:30:00.123456', '2026-02-24 15:30:00')",
        &[],
    )
    .expect("insert agent");
    conn.query_sync(
        "INSERT INTO messages (project_id, sender_id, subject, body_md, created_ts)
         VALUES (1, 1, 'hello', 'body', '2026-02-24')",
        &[],
    )
    .expect("insert message");
    conn.query_sync(
        "INSERT INTO message_recipients (message_id, agent_id, read_ts, ack_ts)
         VALUES (1, 1, NULL, '2026-02-24 15:30:00.123456+00:00')",
        &[],
    )
    .expect("insert message recipient");
    conn.query_sync(
        "INSERT INTO file_reservations (project_id, agent_id, path_pattern, created_ts, expires_ts, released_ts)
         VALUES (1, 1, '*.rs', '1969-12-31 23:59:59', '1970-01-01 00:00:00', '')",
        &[],
    )
    .expect("insert reservation");
    conn.query_sync(
        "INSERT INTO products (product_uid, name, created_at)
         VALUES ('prod-1', 'Product 1', '2026-02-24 15:30:00.123456')",
        &[],
    )
    .expect("insert product");
    conn.query_sync(
        "INSERT INTO product_project_links (product_id, project_id, created_at)
         VALUES (1, 1, '2026-02-24 15:30:00')",
        &[],
    )
    .expect("insert product link");
    conn.query_sync(
        "INSERT INTO agent_links (a_project_id, a_agent_id, b_project_id, b_agent_id, status, created_ts, updated_ts, expires_ts)
         VALUES (1, 1, 1, 1, 'accepted', '2026-02-24 15:30:00.123456', '2026-02-24 15:30:00', NULL)",
        &[],
    )
    .expect("insert agent link");
    conn.query_sync(
        "INSERT INTO project_sibling_suggestions (project_a_id, project_b_id, score, status, rationale, created_ts, evaluated_ts, confirmed_ts, dismissed_ts)
         VALUES (1, 1, 0.9, 'test', 'fixture', '2026-02-24 15:30:00', '2026-02-24 15:30:00.123456', NULL, NULL)",
        &[],
    )
    .expect("insert sibling suggestion");
    conn.query_sync(
        "INSERT INTO fts_messages (message_id, subject, body)
         VALUES (1, 'migration sentinel', 'full migration fixture')",
        &[],
    )
    .expect("insert fts row");
}

fn assert_migrated_fixture_state(conn: &DbConn) {
    let project = conn
        .query_sync(
            "SELECT typeof(created_at) AS t, created_at AS v FROM projects WHERE id = 1",
            &[],
        )
        .expect("query migrated project");
    let project_type: String = project[0].get_named("t").expect("project type");
    let project_value: i64 = project[0].get_named("v").expect("project value");
    assert_eq!(project_type, "integer");
    assert_eq!(project_value, TS_WITH_MICROS);
    assert_eq!(micros_to_iso(project_value), "2026-02-24T15:30:00.123456Z");

    let reservation = conn
        .query_sync(
            "SELECT created_ts AS c, expires_ts AS e, released_ts IS NULL AS released_is_null FROM file_reservations WHERE id = 1",
            &[],
        )
        .expect("query migrated reservation");
    let created_ts: i64 = reservation[0].get_named("c").expect("created_ts");
    let expires_ts: i64 = reservation[0].get_named("e").expect("expires_ts");
    let released_is_null: i64 = reservation[0]
        .get_named("released_is_null")
        .expect("released null marker");
    assert_eq!(created_ts, -1_000_000);
    assert_eq!(expires_ts, 0);
    assert_eq!(released_is_null, 1, "empty released_ts should become NULL");

    let recipient = conn
        .query_sync(
            "SELECT typeof(ack_ts) AS t, ack_ts AS v FROM message_recipients WHERE message_id = 1 AND agent_id = 1",
            &[],
        )
        .expect("query migrated recipient");
    let ack_type: String = recipient[0].get_named("t").expect("ack_ts type");
    let ack_value: i64 = recipient[0].get_named("v").expect("ack_ts value");
    assert_eq!(ack_type, "integer");
    assert_eq!(ack_value, TS_WITH_MICROS);

    // agent_links: created_ts (with micros) and updated_ts (without micros)
    let agent_link = conn
        .query_sync(
            "SELECT typeof(created_ts) AS ct, created_ts AS cv, \
                    typeof(updated_ts) AS ut, updated_ts AS uv, \
                    expires_ts IS NULL AS expires_null \
             FROM agent_links WHERE id = 1",
            &[],
        )
        .expect("query migrated agent_link");
    let al_created_type: String = agent_link[0]
        .get_named("ct")
        .expect("agent_link created type");
    let al_created_val: i64 = agent_link[0]
        .get_named("cv")
        .expect("agent_link created val");
    let al_updated_type: String = agent_link[0]
        .get_named("ut")
        .expect("agent_link updated type");
    let al_updated_val: i64 = agent_link[0]
        .get_named("uv")
        .expect("agent_link updated val");
    let al_expires_null: i64 = agent_link[0]
        .get_named("expires_null")
        .expect("agent_link expires null");
    assert_eq!(
        al_created_type, "integer",
        "agent_links.created_ts should be integer"
    );
    assert_eq!(
        al_created_val, TS_WITH_MICROS,
        "agent_links.created_ts should match expected micros"
    );
    assert_eq!(
        al_updated_type, "integer",
        "agent_links.updated_ts should be integer"
    );
    assert_eq!(
        al_updated_val, TS_NO_FRACTION,
        "agent_links.updated_ts should match expected micros"
    );
    assert_eq!(
        al_expires_null, 1,
        "agent_links.expires_ts NULL should stay NULL"
    );

    // project_sibling_suggestions: created_ts (no micros) and evaluated_ts (with micros)
    let suggestion = conn
        .query_sync(
            "SELECT typeof(created_ts) AS ct, \
                    typeof(evaluated_ts) AS et, \
                    confirmed_ts IS NULL AS confirmed_null, \
                    dismissed_ts IS NULL AS dismissed_null \
             FROM project_sibling_suggestions WHERE id = 1",
            &[],
        )
        .expect("query migrated sibling suggestion");
    let pss_created_type: String = suggestion[0].get_named("ct").expect("pss created type");
    let pss_eval_type: String = suggestion[0].get_named("et").expect("pss evaluated type");
    let pss_confirmed_null: i64 = suggestion[0]
        .get_named("confirmed_null")
        .expect("pss confirmed null");
    let pss_dismissed_null: i64 = suggestion[0]
        .get_named("dismissed_null")
        .expect("pss dismissed null");
    assert_eq!(
        pss_created_type, "integer",
        "project_sibling_suggestions.created_ts should be integer"
    );
    assert_eq!(
        pss_eval_type, "integer",
        "project_sibling_suggestions.evaluated_ts should be integer"
    );
    assert_eq!(
        pss_confirmed_null, 1,
        "project_sibling_suggestions.confirmed_ts NULL should stay NULL"
    );
    assert_eq!(
        pss_dismissed_null, 1,
        "project_sibling_suggestions.dismissed_ts NULL should stay NULL"
    );

    let fts_rows = conn
        .query_sync(
            "SELECT rowid FROM fts_messages WHERE subject MATCH 'sentinel'",
            &[],
        )
        .expect("query fts table after migration");
    assert_eq!(
        fts_rows.len(),
        1,
        "fts query should still return the inserted row"
    );
}

#[test]
fn timestamp_vectors_parse_to_expected_microseconds() {
    assert_eq!(
        parse_vector("2026-02-24 15:30:00.123456"),
        Some(TS_WITH_MICROS)
    );
    assert_eq!(
        parse_vector("2026-02-24T15:30:00.123456"),
        Some(TS_WITH_MICROS)
    );
    assert_eq!(parse_vector("2026-02-24 15:30:00"), Some(TS_NO_FRACTION));
    assert_eq!(parse_vector("2026-02-24"), Some(TS_DATE_ONLY));
    assert_eq!(parse_vector("1970-01-01 00:00:00"), Some(0));
    assert_eq!(parse_vector("1969-12-31 23:59:59"), Some(-1_000_000));
    assert_eq!(
        parse_vector("2026-02-24 15:30:00.123456+00:00"),
        Some(TS_WITH_MICROS)
    );

    let iso = micros_to_iso(TS_WITH_MICROS);
    assert_eq!(iso, "2026-02-24T15:30:00.123456Z");
}

#[test]
fn conversion_handles_null_empty_and_invalid_values() {
    let conn = DbConn::open_memory().expect("open in-memory db");
    conn.execute_raw(
        "CREATE TABLE vectors (
            id INTEGER PRIMARY KEY,
            ts INTEGER
        )",
    )
    .expect("create vectors table");

    conn.query_sync(
        "INSERT INTO vectors (id, ts) VALUES (1, '2026-02-24 15:30:00.123456')",
        &[],
    )
    .expect("insert valid timestamp");
    conn.query_sync("INSERT INTO vectors (id, ts) VALUES (2, '')", &[])
        .expect("insert empty timestamp");
    conn.query_sync("INSERT INTO vectors (id, ts) VALUES (3, 'not-a-date')", &[])
        .expect("insert invalid timestamp");
    conn.query_sync("INSERT INTO vectors (id, ts) VALUES (4, NULL)", &[])
        .expect("insert null timestamp");

    let result = convert_column(&conn, "vectors", "ts").expect("convert vectors.ts");
    assert_eq!(result.converted, 1, "one valid row should convert");
    assert_eq!(result.nulls, 1, "empty string should convert to NULL");
    assert_eq!(result.skipped, 1, "invalid timestamp should be skipped");
    assert!(
        result.errors.iter().any(|msg| msg.contains("not-a-date")),
        "parse errors should include the invalid value"
    );

    let row1 = conn
        .query_sync(
            "SELECT typeof(ts) AS t, ts AS v FROM vectors WHERE id = 1",
            &[],
        )
        .expect("query row 1");
    let row1_type: String = row1[0].get_named("t").expect("row1 type");
    let row1_value: i64 = row1[0].get_named("v").expect("row1 value");
    assert_eq!(row1_type, "integer");
    assert_eq!(row1_value, TS_WITH_MICROS);

    let row2 = conn
        .query_sync(
            "SELECT ts IS NULL AS is_null FROM vectors WHERE id = 2",
            &[],
        )
        .expect("query row 2");
    let row2_is_null: i64 = row2[0].get_named("is_null").expect("row2 is_null");
    assert_eq!(row2_is_null, 1, "empty string should become NULL");

    let row3 = conn
        .query_sync(
            "SELECT typeof(ts) AS t, ts AS v FROM vectors WHERE id = 3",
            &[],
        )
        .expect("query row 3");
    let row3_type: String = row3[0].get_named("t").expect("row3 type");
    let row3_value: String = row3[0].get_named("v").expect("row3 value");
    assert_eq!(row3_type, "text", "invalid row should remain text");
    assert_eq!(row3_value, "not-a-date");

    let row4 = conn
        .query_sync(
            "SELECT ts IS NULL AS is_null FROM vectors WHERE id = 4",
            &[],
        )
        .expect("query row 4");
    let row4_is_null: i64 = row4[0].get_named("is_null").expect("row4 is_null");
    assert_eq!(row4_is_null, 1, "NULL should be preserved");
}

#[test]
fn full_migration_fixture_is_idempotent_and_preserves_fts5_queries() {
    let conn = DbConn::open_memory().expect("open in-memory db");
    conn.execute_raw(CREATE_TABLES_SQL).expect("create schema");
    seed_full_migration_fixture(&conn);

    let detected_before = detect_timestamp_format(&conn).expect("detect pre-migration format");
    assert_eq!(detected_before, TimestampFormat::PythonText);

    let first = convert_all_timestamps(&conn).expect("first migration pass");
    assert!(
        first.success,
        "migration should succeed without skipped rows"
    );
    assert_eq!(first.total_skipped, 0);
    assert!(first.total_converted > 0);

    assert_migrated_fixture_state(&conn);

    let detected_after = detect_timestamp_format(&conn).expect("detect post-migration format");
    assert_eq!(detected_after, TimestampFormat::RustMicros);

    let second = convert_all_timestamps(&conn).expect("second migration pass");
    assert!(second.success);
    assert_eq!(second.total_converted, 0, "second pass must be idempotent");
    assert_eq!(second.total_skipped, 0);
}
