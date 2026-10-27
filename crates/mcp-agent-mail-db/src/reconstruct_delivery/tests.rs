use super::*;
use crate::sync::{InboxDeliveryEventError, inbox_delivery_events_from_conn};

fn mailbox(conn: &CanonicalDbConn, offset: i64) {
    conn.execute_raw(crate::schema::PRAGMA_DB_INIT_BASE_SQL)
        .unwrap();
    conn.execute_raw(&crate::schema::init_schema_sql_base())
        .unwrap();
    for migration in crate::schema::schema_migrations_base() {
        if migration.id.starts_with("v25_")
            || migration.id == "v33_project_scoped_inbox_delivery_events"
            || migration.id.starts_with("v34_")
        {
            conn.execute_raw(&migration.up).unwrap();
        }
    }
    for (project, slug) in [(1, "a"), (2, "z")] {
        execute(
            conn,
            "INSERT INTO projects(id, slug, human_key, created_at) VALUES (?, ?, ?, 1)",
            &[
                Value::BigInt(project + offset),
                Value::Text(slug.into()),
                Value::Text(format!("/{slug}")),
            ],
        )
        .unwrap();
    }
    for (agent, project, name) in [
        (11, 1, "Alice"),
        (12, 1, "Bob"),
        (21, 2, "Carol"),
        (22, 2, "Dave"),
    ] {
        execute(conn,
            "INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (?, ?, ?, 'test', 'test', 1, 1)",
            &[Value::BigInt(agent + offset), Value::BigInt(project + offset), Value::Text(name.into())]).unwrap();
    }
}

fn message(conn: &CanonicalDbConn, offset: i64, id: i64, project: i64, sender: i64) {
    execute(conn,
        "INSERT INTO messages(id, project_id, sender_id, subject, body_md, created_ts) VALUES (?, ?, ?, ?, 'body', ?)",
        &[Value::BigInt(id + offset), Value::BigInt(project + offset), Value::BigInt(sender + offset), Value::Text(format!("message-{id}")), Value::BigInt(id)]).unwrap();
}

fn direct(conn: &CanonicalDbConn, offset: i64, id: i64, project: i64, sender: i64, recipient: i64) {
    message(conn, offset, id, project, sender);
    execute(
        conn,
        "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES (?, ?, 'to')",
        &[
            Value::BigInt(id + offset),
            Value::BigInt(recipient + offset),
        ],
    )
    .unwrap();
}

fn shared(conn: &CanonicalDbConn, offset: i64, id: i64, seq: i64) {
    message(conn, offset, id, 1, 11);
    execute(conn, "INSERT INTO project_mailbox_deliveries(message_id, project_id, kind, delivered_ts) VALUES (?, ?, 'to', ?)",
        &[Value::BigInt(id + offset), Value::BigInt(1 + offset), Value::BigInt(id)]).unwrap();
    execute(conn, "INSERT INTO inbox_delivery_events(seq, project_id, agent_id, message_id, kind, delivered_ts) VALUES (?, ?, NULL, ?, 'project', ?)",
        &[Value::BigInt(seq), Value::BigInt(1 + offset), Value::BigInt(id + offset), Value::BigInt(id)]).unwrap();
}

fn ceiling(conn: &CanonicalDbConn, value: i64) {
    conn.execute_raw("DELETE FROM sqlite_sequence WHERE name = 'inbox_delivery_events'")
        .unwrap();
    execute(
        conn,
        "INSERT INTO sqlite_sequence(name, seq) VALUES ('inbox_delivery_events', ?)",
        &[Value::BigInt(value)],
    )
    .unwrap();
}

type TestMaps = (HashMap<i64, i64>, HashMap<i64, i64>, HashMap<i64, i64>);

fn maps(message_ids: &[i64]) -> TestMaps {
    (
        [(1001, 1), (1002, 2)].into_iter().collect(),
        [(1011, 11), (1012, 12), (1021, 21), (1022, 22)]
            .into_iter()
            .collect(),
        message_ids.iter().map(|id| (id + 1000, *id)).collect(),
    )
}

fn snapshot(conn: &CanonicalDbConn) -> Vec<Vec<Vec<Value>>> {
    [
        "SELECT * FROM inbox_delivery_events ORDER BY seq",
        "SELECT * FROM inbox_delivery_cursor_state ORDER BY project_id, scope, scope_id",
        "SELECT * FROM sqlite_sequence ORDER BY name",
        "SELECT * FROM project_mailbox_deliveries ORDER BY message_id",
    ]
    .iter()
    .map(|sql| {
        rows(conn, sql, &[])
            .unwrap()
            .iter()
            .map(|row| row.values().cloned().collect())
            .collect()
    })
    .collect()
}

#[test]
fn salvage_preserves_global_order_and_appends_archive_only_events_above_issued_ceiling() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.sqlite3");
    let source = CanonicalDbConn::open_file(source_path.to_str().unwrap()).unwrap();
    let target = CanonicalDbConn::open_memory().unwrap();
    mailbox(&source, 1000);
    mailbox(&target, 0);
    for (id, project, sender, recipient) in [
        (101, 1, 11, 12),
        (102, 2, 21, 22),
        (103, 1, 11, 12),
        (104, 1, 11, 12),
    ] {
        direct(&source, 1000, id, project, sender, recipient);
    }
    // Archive replay groups by project and cannot reproduce global order.
    for (id, project, sender, recipient) in [
        (101, 1, 11, 12),
        (103, 1, 11, 12),
        (104, 1, 11, 12),
        (102, 2, 21, 22),
    ] {
        direct(&target, 0, id, project, sender, recipient);
    }
    let wrong = inbox_delivery_events_from_conn(&target, 1, 12, Some(3), 10).unwrap();
    assert_eq!(
        wrong.events.len(),
        0,
        "replay alone silently skips message 104"
    );
    direct(&target, 0, 105, 1, 11, 12);
    ceiling(&source, 8000);
    source.execute_raw("PRAGMA query_only = ON").unwrap();
    let source_before = std::fs::read(&source_path).unwrap();
    let (projects, agents, messages) = maps(&[101, 102, 103, 104]);

    preserve_delivery_history(&target, &source, &projects, &agents, &messages).unwrap();

    let page = inbox_delivery_events_from_conn(&target, 1, 12, Some(3), 10).unwrap();
    assert_eq!(
        page.events
            .iter()
            .map(|event| (event.seq, event.message_id))
            .collect::<Vec<_>>(),
        vec![(4, 104), (8001, 105)]
    );
    assert_eq!(page.next_cursor, 8001);
    assert_eq!(page.tail_cursor, 8001);
    let other = inbox_delivery_events_from_conn(&target, 2, 22, None, 10).unwrap();
    assert_eq!(
        other
            .events
            .iter()
            .map(|event| (event.seq, event.message_id))
            .collect::<Vec<_>>(),
        vec![(2, 102)]
    );
    assert_eq!(
        std::fs::read(&source_path).unwrap(),
        source_before,
        "salvage source must remain byte-identical"
    );
    assert_eq!(
        rows(&target, "PRAGMA foreign_key_check", &[])
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn salvage_keeps_fully_pruned_tail_without_resurrecting_archived_delivery() {
    let source = CanonicalDbConn::open_memory().unwrap();
    let target = CanonicalDbConn::open_memory().unwrap();
    mailbox(&source, 1000);
    mailbox(&target, 0);
    ceiling(&source, 99);
    direct(&source, 1000, 101, 1, 11, 12);
    source
        .execute_raw("DELETE FROM inbox_delivery_events")
        .unwrap();
    direct(&target, 0, 101, 1, 11, 12);
    let (projects, agents, messages) = maps(&[101]);

    preserve_delivery_history(&target, &source, &projects, &agents, &messages).unwrap();

    assert!(matches!(
        inbox_delivery_events_from_conn(&target, 1, 12, Some(99), 10),
        Err(InboxDeliveryEventError::CursorExpired { .. })
    ));
    let page = inbox_delivery_events_from_conn(&target, 1, 12, Some(100), 10).unwrap();
    assert_eq!(page.events.len(), 0);
    assert_eq!(page.tail_cursor, 100);
    assert_eq!(page.next_cursor, 100);
    direct(&target, 0, 102, 1, 11, 12);
    let page = inbox_delivery_events_from_conn(&target, 1, 12, Some(100), 10).unwrap();
    assert_eq!(page.events[0].seq, 101);
    assert_eq!(page.events[0].message_id, 102);
}

#[test]
fn salvage_preserves_shared_events_and_viewer_loss_without_replaying_pruned_shared_mail() {
    let source = CanonicalDbConn::open_memory().unwrap();
    let target = CanonicalDbConn::open_memory().unwrap();
    mailbox(&source, 1000);
    mailbox(&target, 0);
    shared(&source, 1000, 101, 77);
    shared(&source, 1000, 102, 88);
    source
        .execute_raw("DELETE FROM inbox_delivery_events WHERE seq = 88")
        .unwrap();
    shared(&target, 0, 101, 1);
    shared(&target, 0, 102, 2);
    let (projects, agents, messages) = maps(&[101, 102]);

    preserve_delivery_history(&target, &source, &projects, &agents, &messages).unwrap();

    let page = inbox_delivery_events_from_conn(&target, 1, 12, None, 10).unwrap();
    assert_eq!(
        page.events
            .iter()
            .map(|event| (event.seq, event.message_id))
            .collect::<Vec<_>>(),
        vec![(77, 101)]
    );
    assert_eq!(page.tail_cursor, 88);
    assert!(matches!(
        inbox_delivery_events_from_conn(&target, 1, 12, Some(77), 10),
        Err(InboxDeliveryEventError::CursorExpired { .. })
    ));
    assert_eq!(
        inbox_delivery_events_from_conn(&target, 1, 12, Some(88), 10)
            .unwrap()
            .events
            .len(),
        0
    );
    assert_eq!(
        rows(&target, "SELECT * FROM project_mailbox_deliveries", &[])
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        rows(&target, "PRAGMA foreign_key_check", &[])
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn salvage_legacy_ledger_preserves_issued_tail_without_inventing_loss() {
    let source = CanonicalDbConn::open_memory().unwrap();
    let target = CanonicalDbConn::open_memory().unwrap();
    mailbox(&source, 1000);
    mailbox(&target, 0);
    direct(&source, 1000, 101, 1, 11, 12);
    direct(&target, 0, 101, 1, 11, 12);
    source
        .execute_raw(
            "DROP TRIGGER trg_inbox_delivery_cursor_insert;
         DROP TRIGGER trg_inbox_delivery_cursor_delete;
         DROP TRIGGER trg_messages_cascade_inbox_delivery_events;
         DROP TABLE inbox_delivery_cursor_state;",
        )
        .unwrap();
    ceiling(&source, 700);
    let (projects, agents, messages) = maps(&[101]);

    preserve_delivery_history(&target, &source, &projects, &agents, &messages).unwrap();

    let page = inbox_delivery_events_from_conn(&target, 1, 12, Some(0), 10).unwrap();
    assert_eq!(page.events[0].seq, 1);
    assert_eq!(page.tail_cursor, 700);
    assert_eq!(
        inbox_delivery_events_from_conn(&target, 1, 12, Some(700), 10)
            .unwrap()
            .events
            .len(),
        0
    );
    direct(&target, 0, 102, 1, 11, 12);
    assert_eq!(
        inbox_delivery_events_from_conn(&target, 1, 12, Some(700), 10)
            .unwrap()
            .events[0]
            .seq,
        701
    );
}

#[test]
fn salvage_cursor_ambiguity_and_sequence_exhaustion_roll_back_candidate() {
    for fault in [
        "collapsed deliveries",
        "unmapped scope",
        "sequence exhausted",
        "missing state",
    ] {
        let source = CanonicalDbConn::open_memory().unwrap();
        let target = CanonicalDbConn::open_memory().unwrap();
        mailbox(&source, 1000);
        mailbox(&target, 0);
        direct(&source, 1000, 101, 1, 11, 12);
        direct(&target, 0, 101, 1, 11, 12);
        let (projects, agents, mut messages) = maps(&[101]);
        match fault {
            "collapsed deliveries" => {
                direct(&source, 1000, 102, 1, 11, 12);
                messages.insert(1102, 101);
            }
            "unmapped scope" => source
                .execute_raw(
                    "INSERT INTO inbox_delivery_cursor_state VALUES(1001, 'agent', 9999, 99, 50)",
                )
                .unwrap(),
            "sequence exhausted" => {
                ceiling(&source, i64::MAX);
                direct(&target, 0, 102, 1, 11, 12);
            }
            "missing state" => source
                .execute_raw("DELETE FROM inbox_delivery_cursor_state")
                .unwrap(),
            _ => unreachable!(),
        }
        let before = snapshot(&target);
        target.execute_raw("BEGIN IMMEDIATE").unwrap();
        let error = preserve_delivery_history(&target, &source, &projects, &agents, &messages)
            .expect_err(fault);
        assert!(
            error
                .to_string()
                .contains("cannot preserve inbox delivery history"),
            "{fault}: {error}"
        );
        assert_eq!(
            snapshot(&target),
            before,
            "{fault} must restore all candidate authority"
        );
        target.execute_raw("COMMIT").unwrap();
        assert_eq!(
            rows(
                &target,
                "SELECT name FROM sqlite_temp_master WHERE name LIKE 'reconstruct_%'",
                &[]
            )
            .unwrap()
            .len(),
            0
        );
    }
}

#[test]
fn salvage_refuses_missing_declared_authority_and_legacy_identity_collapse() {
    for fault in [
        "missing ledger",
        "missing state table",
        "orphan sequence",
        "missing shared authority",
        "legacy agent collapse",
        "legacy project collapse",
    ] {
        let source = CanonicalDbConn::open_memory().unwrap();
        let target = CanonicalDbConn::open_memory().unwrap();
        mailbox(&source, 1000);
        mailbox(&target, 0);
        direct(&source, 1000, 101, 1, 11, 12);
        direct(&target, 0, 101, 1, 11, 12);
        let (mut projects, mut agents, messages) = maps(&[101]);
        source
            .execute_raw(
                "DROP TRIGGER trg_inbox_delivery_cursor_insert;
             DROP TRIGGER trg_inbox_delivery_cursor_delete;",
            )
            .unwrap();
        match fault {
            "missing ledger" => {
                source
                    .execute_raw(
                        "DROP TABLE inbox_delivery_events; DROP TABLE inbox_delivery_cursor_state",
                    )
                    .unwrap();
                source.execute_raw(&format!(
                    "CREATE TABLE {}(id TEXT PRIMARY KEY); INSERT INTO {} VALUES('v25_create_inbox_delivery_events')",
                    crate::schema::MIGRATIONS_TABLE_NAME, crate::schema::MIGRATIONS_TABLE_NAME,
                )).unwrap();
            }
            "missing state table" => {
                source
                    .execute_raw("DROP TABLE inbox_delivery_cursor_state")
                    .unwrap();
                source.execute_raw(&format!(
                    "CREATE TABLE {}(id TEXT PRIMARY KEY); INSERT INTO {} VALUES('v34_create_inbox_delivery_cursor_state')",
                    crate::schema::MIGRATIONS_TABLE_NAME, crate::schema::MIGRATIONS_TABLE_NAME,
                )).unwrap();
            }
            "orphan sequence" => {
                source
                    .execute_raw(
                        "DROP TABLE inbox_delivery_events; DROP TABLE inbox_delivery_cursor_state",
                    )
                    .unwrap();
                ceiling(&source, 77);
            }
            "missing shared authority" => source
                .execute_raw("DROP TABLE project_mailbox_deliveries")
                .unwrap(),
            "legacy agent collapse" => {
                source
                    .execute_raw("DROP TRIGGER trg_messages_cascade_inbox_delivery_events")
                    .unwrap();
                source
                    .execute_raw("DROP TABLE inbox_delivery_cursor_state")
                    .unwrap();
                agents.insert(1011, 12);
            }
            "legacy project collapse" => {
                source
                    .execute_raw("DROP TRIGGER trg_messages_cascade_inbox_delivery_events")
                    .unwrap();
                source
                    .execute_raw("DROP TABLE inbox_delivery_cursor_state")
                    .unwrap();
                projects.insert(1002, 1);
            }
            _ => unreachable!(),
        }
        let before = snapshot(&target);
        let error = preserve_delivery_history(&target, &source, &projects, &agents, &messages)
            .expect_err(fault);
        assert!(
            error
                .to_string()
                .contains("cannot preserve inbox delivery history"),
            "{fault}: {error}"
        );
        assert_eq!(snapshot(&target), before, "{fault}");
    }
}

#[test]
fn salvage_baseline_without_issuance_witness_remains_compatible() {
    let source = CanonicalDbConn::open_memory().unwrap();
    let target = CanonicalDbConn::open_memory().unwrap();
    source
        .execute_raw(&crate::schema::init_schema_sql_base())
        .unwrap();
    mailbox(&target, 0);
    direct(&target, 0, 101, 1, 11, 12);
    let before = snapshot(&target);
    let empty = HashMap::new();
    preserve_delivery_history(&target, &source, &empty, &empty, &empty).unwrap();
    assert_eq!(snapshot(&target), before);

    source
        .execute_raw(
            "INSERT INTO projects(id,slug,human_key,created_at) VALUES(1,'a','/a',1);
         INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts)
         VALUES(11,1,'Alice','test','test',1,1);",
        )
        .unwrap();
    message(&source, 0, 101, 1, 11);
    source
        .execute_raw("INSERT INTO project_mailbox_deliveries VALUES(101,1,'to',101)")
        .unwrap();
    let error = preserve_delivery_history(&target, &source, &empty, &empty, &empty)
        .expect_err("nonempty post-cursor delivery authority cannot pass as an old baseline");
    assert!(
        error.to_string().contains("delivery ledger is missing"),
        "{error}"
    );
    assert_eq!(snapshot(&target), before);
}

#[test]
fn public_salvage_restores_source_only_shared_event_receipt_and_recipient_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.sqlite3");
    let target_path = directory.path().join("candidate.sqlite3");
    let storage_root = directory.path().join("archive");
    std::fs::create_dir_all(storage_root.join("projects")).unwrap();
    let source = CanonicalDbConn::open_file(source_path.to_str().unwrap()).unwrap();
    mailbox(&source, 1000);
    shared(&source, 1000, 101, 777);
    source.execute_raw("INSERT INTO project_mailbox_receipts(message_id,agent_id,read_ts,ack_ts) VALUES(1101,1012,800,900)").unwrap();
    // The DB-only delivery has no archive artifact and its message predates
    // recipients_json persistence. The delivery table is the surviving fact.
    source
        .execute_raw("UPDATE messages SET recipients_json = '{}' WHERE id = 1101")
        .unwrap();
    drop(source);
    let source_before = std::fs::read(&source_path).unwrap();

    let stats = crate::reconstruct::reconstruct_from_archive_with_private_salvage(
        &target_path,
        &storage_root,
        &source_path,
    )
    .unwrap();

    assert_eq!(stats.salvaged_project_mailbox_receipts, 1);
    let target = CanonicalDbConn::open_file(target_path.to_str().unwrap()).unwrap();
    let identity = rows(&target,
        "SELECT a.id AS agent_id, p.id AS project_id FROM agents a JOIN projects p ON p.id = a.project_id WHERE p.slug = 'a' AND a.name = 'Bob'", &[]).unwrap();
    let agent = integer(&identity[0], "agent_id").unwrap();
    let project = integer(&identity[0], "project_id").unwrap();
    assert_ne!(agent, 1012, "public merge must exercise identity remapping");
    let page = inbox_delivery_events_from_conn(&target, project, agent, Some(776), 10).unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].seq, 777);
    assert_eq!(page.events[0].message_id, 1101);
    let receipt = rows(&target, "SELECT read_ts, ack_ts FROM project_mailbox_receipts WHERE message_id = 1101 AND agent_id = ?", &[Value::BigInt(agent)]).unwrap();
    assert_eq!(integer(&receipt[0], "read_ts").unwrap(), 800);
    assert_eq!(integer(&receipt[0], "ack_ts").unwrap(), 900);
    let recipient_json = rows(
        &target,
        "SELECT recipients_json FROM messages WHERE id = 1101",
        &[],
    )
    .unwrap();
    let recipients: serde_json::Value =
        serde_json::from_str(&text(&recipient_json[0], "recipients_json").unwrap()).unwrap();
    assert_eq!(recipients["to"], serde_json::json!(["project:a"]));
    assert_eq!(std::fs::read(&source_path).unwrap(), source_before);
    assert_eq!(
        rows(&target, "PRAGMA foreign_key_check", &[])
            .unwrap()
            .len(),
        0
    );
}
