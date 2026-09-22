//! Regression controls for explorer candidate selection and page boundaries.
use super::*;

fn entry(id: i64, timestamp: i64, direction: Direction) -> ExplorerEntry {
    ExplorerEntry {
        message_id: id,
        project_id: 1,
        project_slug: "project".to_string(),
        sender_name: "Alpha".to_string(),
        to_agents: "Alpha".to_string(),
        subject: String::new(),
        body_md: String::new(),
        thread_id: None,
        importance: "normal".to_string(),
        ack_required: false,
        created_ts: timestamp,
        kind: None,
        read_ts: None,
        ack_ts: None,
        direction,
    }
}

#[test]
fn candidate_limit_rejects_overflow_instead_of_unlimited_sql() {
    let query = ExplorerQuery {
        limit: 2,
        offset: usize::MAX,
        ..Default::default()
    };
    assert!(matches!(
        candidate_limit(&query),
        Err(DbError::InvalidArgument {
            field: "pagination",
            ..
        })
    ));
    if let Ok(offset) = usize::try_from(i64::MAX) {
        assert!(
            candidate_limit(&ExplorerQuery {
                limit: 1,
                offset,
                ..Default::default()
            })
            .is_err()
        );
    }
}

#[test]
fn candidate_limit_preserves_zero_and_offset_semantics() {
    assert_eq!(
        candidate_limit(&ExplorerQuery {
            limit: 0,
            offset: usize::MAX,
            ..Default::default()
        })
        .unwrap(),
        0
    );
    assert_eq!(
        candidate_limit(&ExplorerQuery {
            limit: 7,
            offset: 13,
            ..Default::default()
        })
        .unwrap(),
        20
    );
}

#[test]
fn equal_timestamp_pages_have_deterministic_message_order() {
    for mode in [
        SortMode::DateDesc,
        SortMode::ImportanceDesc,
        SortMode::AgentAlpha,
    ] {
        let mut entries = vec![
            entry(1, 10, Direction::Outbound),
            entry(3, 10, Direction::Inbound),
            entry(2, 10, Direction::Inbound),
            entry(1, 10, Direction::Inbound),
        ];
        sort_entries(&mut entries, mode);
        assert_eq!(
            entries.iter().map(|e| e.message_id).collect::<Vec<_>>(),
            [3, 2, 1, 1]
        );
        assert_eq!(entries[2].direction, Direction::Inbound);
        assert_eq!(entries[3].direction, Direction::Outbound);
        entries.reverse();
        sort_entries(&mut entries, mode);
        assert_eq!(
            entries.iter().map(|e| e.message_id).collect::<Vec<_>>(),
            [3, 2, 1, 1]
        );
    }
    let mut entries = vec![
        entry(3, 10, Direction::Inbound),
        entry(1, 10, Direction::Inbound),
    ];
    sort_entries(&mut entries, SortMode::DateAsc);
    assert_eq!(entries[0].message_id, 1);
}

#[test]
fn sql_orders_match_requested_mode_and_merge_tie_breaker() {
    assert_eq!(
        sql_order_by(SortMode::DateAsc, true),
        "m.created_ts ASC, m.id ASC"
    );
    assert_eq!(
        sql_order_by(SortMode::DateDesc, false),
        "m.created_ts DESC, m.id DESC"
    );
    assert!(
        sql_order_by(SortMode::ImportanceDesc, true).starts_with("CASE m.importance COLLATE BINARY")
    );
    assert!(sql_order_by(SortMode::AgentAlpha, true).starts_with("sender_name COLLATE NOCASE"));
    assert!(sql_order_by(SortMode::AgentAlpha, false).starts_with("to_agents COLLATE NOCASE"));
}

fn inbound_values() -> Vec<Value> {
    vec![
        Value::BigInt(1),
        Value::BigInt(1),
        Value::BigInt(2),
        Value::Null,
        Value::Text("subject".to_string()),
        Value::Text("body".to_string()),
        Value::Text("normal".to_string()),
        Value::BigInt(1),
        Value::BigInt(100),
        Value::Text("to".to_string()),
        Value::Null,
        Value::Null,
        Value::Text("Sender".to_string()),
        Value::Text("project".to_string()),
        Value::Text("Recipient".to_string()),
    ]
}

fn positional_row(values: Vec<Value>) -> SqlRow {
    SqlRow::new(
        (0..values.len())
            .map(|index| format!("column_{index}"))
            .collect(),
        values,
    )
}

#[test]
fn valid_nullable_inbox_fields_are_not_errors() {
    let decoded = map_inbound_row(&positional_row(inbound_values())).unwrap();
    assert_eq!(decoded.thread_id, None);
    assert_eq!(decoded.read_ts, None);
    assert_eq!(decoded.ack_ts, None);
    assert_eq!(decoded.kind.as_deref(), Some("to"));
    assert!(decoded.ack_required);
}

#[test]
fn malformed_inbox_projection_is_not_a_missing_message() {
    for column in [0, 1, 4, 5, 6, 7, 8, 9, 12, 13, 14] {
        let mut values = inbound_values();
        values[column] = Value::Null;
        assert!(
            map_inbound_row(&positional_row(values)).is_err(),
            "column {column}"
        );
    }
    for column in [10, 11] {
        let mut values = inbound_values();
        values[column] = Value::Text("not-a-timestamp".to_string());
        assert!(map_inbound_row(&positional_row(values)).is_err());
    }
    let mut values = inbound_values();
    values[3] = Value::BigInt(99);
    assert!(map_inbound_row(&positional_row(values)).is_err());
    assert!(map_inbound_row(&positional_row(Vec::new())).is_err());
}

#[test]
fn malformed_outbox_projection_is_not_a_default_message() {
    let incoming = inbound_values();
    let mut outgoing = incoming[..9].to_vec();
    outgoing.extend_from_slice(&incoming[12..]);
    assert!(map_outbound_row(&positional_row(outgoing.clone())).is_ok());
    for column in [0, 1, 4, 5, 6, 7, 8, 9, 10, 11] {
        let mut values = outgoing.clone();
        values[column] = Value::Null;
        assert!(
            map_outbound_row(&positional_row(values)).is_err(),
            "column {column}"
        );
    }
}

#[test]
fn count_projection_distinguishes_zero_from_unavailable() {
    let count = |value| SqlRow::new(vec!["cnt".to_string()], vec![value]);
    assert_eq!(map_count_rows(&[count(Value::BigInt(0))]).unwrap(), 0);
    assert_eq!(map_count_rows(&[count(Value::BigInt(42))]).unwrap(), 42);
    assert!(map_count_rows(&[]).is_err());
    assert!(map_count_rows(&[count(Value::Null)]).is_err());
    assert!(map_count_rows(&[count(Value::Text("bad".to_string()))]).is_err());
    assert!(map_count_rows(&[count(Value::BigInt(-1))]).is_err());
}

#[test]
fn malformed_agent_identity_is_not_an_absent_agent() {
    let row = SqlRow::new(
        vec!["project_id".to_string(), "id".to_string()],
        vec![Value::BigInt(1), Value::BigInt(2)],
    );
    assert_eq!(map_agent_row(&row).unwrap(), (1, 2));
    let row = SqlRow::new(
        vec!["project_id".to_string(), "id".to_string()],
        vec![Value::BigInt(1), Value::Null],
    );
    assert!(map_agent_row(&row).is_err());
    assert!(map_agent_row(&positional_row(Vec::new())).is_err());
}
