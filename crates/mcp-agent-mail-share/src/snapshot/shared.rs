//! Preserve project-mailbox delivery and receipt authority in snapshots.
//!
//! These are not broadcast recipient rows. Copy one delivery per message and
//! only the receipts that actually exist, in the parent's source transaction.
//! Sharing presets subsequently clear private receipts; private archive copies
//! retain their exact integer timestamps rather than the legacy text encoding.

use std::collections::{BTreeMap, HashSet};

use super::{CanonicalDbConn, KnownTable, ShareError, SnapshotSource, Value, create_dst_table};

const PAGE_SIZE: usize = 1000;
const TABLES: &[KnownTable] = &[
    KnownTable {
        name: "project_mailbox_deliveries",
        page_by_column: Some("message_id"),
        primary_key_columns: &["message_id"],
        columns: &["message_id", "project_id", "kind", "delivered_ts"],
    },
    KnownTable {
        name: "project_mailbox_receipts",
        page_by_column: None,
        primary_key_columns: &["message_id", "agent_id"],
        columns: &["message_id", "agent_id", "read_ts", "ack_ts"],
    },
];

fn invalid(message: impl Into<String>) -> ShareError {
    ShareError::Validation {
        message: message.into(),
    }
}

fn query(
    src: &impl SnapshotSource,
    sql: &str,
    params: &[Value],
) -> Result<Vec<sqlmodel_core::Row>, ShareError> {
    src.query_snapshot(sql, params)
        .map_err(|error| ShareError::Sqlite {
            message: format!("read shared mailbox snapshot: {error}"),
        })
}

/// A missing receipt table is not permission to export every delivery as
/// unacknowledged. Only the wholly absent pre-mailbox schema may be skipped.
/// Inspect both schemas even when the delivery table currently has no rows.
/// Retained project delivery events contradict the pre-mailbox exception.
fn source_has_shared_schema(src: &impl SnapshotSource) -> Result<bool, ShareError> {
    let objects = query(
        src,
        "SELECT name, type FROM sqlite_master WHERE name COLLATE NOCASE \
         IN ('project_mailbox_deliveries', 'project_mailbox_receipts')",
        &[],
    )?;
    if objects.is_empty() {
        verify_legacy_shared_absence(src)?;
        return Ok(false);
    }
    let mut present = HashSet::new();
    for object in objects {
        let name = object
            .get_named::<String>("name")
            .map_err(|_| invalid("shared snapshot catalog name is not text"))?
            .to_ascii_lowercase();
        let kind = object
            .get_named::<String>("type")
            .map_err(|_| invalid("shared snapshot catalog type is not text"))?;
        if kind != "table"
            || !TABLES.iter().any(|table| table.name == name)
            || !present.insert(name)
        {
            return Err(invalid("shared snapshot authority must be two real tables"));
        }
    }
    if present.len() != TABLES.len() {
        return Err(invalid(
            "shared snapshot delivery/receipt schema is incomplete",
        ));
    }
    for table in TABLES {
        let rows = query(src, &format!("PRAGMA table_info(\"{}\")", table.name), &[])?;
        let mut columns = HashSet::new();
        let mut key = BTreeMap::new();
        for row in rows {
            let name = row
                .get_named::<String>("name")
                .map_err(|_| invalid("shared snapshot column name is not text"))?
                .to_ascii_lowercase();
            let position = row
                .get_named::<i64>("pk")
                .map_err(|_| invalid("shared snapshot primary-key position is not an integer"))?;
            if position < 0 || !columns.insert(name.clone()) {
                return Err(invalid("shared snapshot column identity is invalid"));
            }
            if position > 0 && key.insert(position, name).is_some() {
                return Err(invalid("shared snapshot primary key is ambiguous"));
            }
        }
        if table
            .columns
            .iter()
            .any(|column| !columns.contains(*column))
            || key.len() != table.primary_key_columns.len()
            || key.iter().zip(table.primary_key_columns).enumerate().any(
                |(index, ((position, name), expected))| {
                    *position != i64::try_from(index + 1).unwrap_or(i64::MAX)
                        || name.as_str() != *expected
                },
            )
        {
            return Err(invalid(format!(
                "shared snapshot table {} lacks its complete columns or exact primary key",
                table.name
            )));
        }
    }
    Ok(true)
}

/// A known-table export deliberately omits the delivery event ledger. Check
/// its evidence before omitting missing shared tables, or the output erases
/// the last indication that shared routing and acknowledgements were lost.
/// This executes in the parent's source read transaction and never repairs it.
fn verify_legacy_shared_absence(src: &impl SnapshotSource) -> Result<(), ShareError> {
    let objects = query(
        src,
        "SELECT type FROM sqlite_master WHERE name = 'inbox_delivery_events' COLLATE NOCASE",
        &[],
    )?;
    if objects.is_empty() {
        return Ok(());
    }
    if objects.len() != 1
        || objects[0]
            .get_named::<String>("type")
            .map_err(|_| invalid("shared snapshot event catalog type is not text"))?
            != "table"
    {
        return Err(invalid(
            "shared snapshot event authority must be a real table",
        ));
    }
    if !query(
        src,
        "SELECT seq FROM inbox_delivery_events WHERE kind = 'project' LIMIT 1",
        &[],
    )?
    .is_empty()
    {
        return Err(invalid(
            "shared snapshot tables are missing but retained project delivery events exist; refusing lossy copy",
        ));
    }
    Ok(())
}

fn page_query(table: &KnownTable, after: Option<(i64, i64)>) -> (String, Vec<Value>) {
    // All identifiers are internal constants. Explicit aliases retain the
    // expected result-column spelling even on an imported upper-case schema.
    let columns = table
        .columns
        .iter()
        .map(|column| format!("\"{column}\" AS \"{column}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let composite = table.primary_key_columns.len() == 2;
    let mut sql = format!("SELECT {columns} FROM \"{}\"", table.name);
    let mut params = Vec::new();
    if let Some((message, agent)) = after {
        if composite {
            sql.push_str(" WHERE message_id > ?1 OR (message_id = ?1 AND agent_id > ?2)");
            params.extend([Value::BigInt(message), Value::BigInt(agent)]);
        } else {
            sql.push_str(" WHERE message_id > ?1");
            params.push(Value::BigInt(message));
        }
    }
    sql.push_str(" ORDER BY message_id ASC");
    if composite {
        sql.push_str(", agent_id ASC");
    }
    sql.push_str(" LIMIT 1000");
    (sql, params)
}

fn values_and_key(
    table: &KnownTable,
    row: &sqlmodel_core::Row,
) -> Result<(Vec<Value>, (i64, i64)), ShareError> {
    let mut values = Vec::with_capacity(table.columns.len());
    let mut key = (0, 0);
    for &column in table.columns {
        let value = row.get_by_name(column).ok_or_else(|| {
            invalid(format!(
                "shared snapshot is missing {}.{column}",
                table.name
            ))
        })?;
        match (column, value) {
            ("kind", Value::Text(kind)) if matches!(kind.as_str(), "to" | "cc") => {}
            ("read_ts" | "ack_ts", Value::Null) => {}
            (_, Value::BigInt(number)) if column != "kind" => {
                if column == "message_id" {
                    key.0 = *number;
                } else if column == "agent_id" {
                    key.1 = *number;
                }
            }
            (_, Value::Int(number)) if column != "kind" => {
                if column == "message_id" {
                    key.0 = i64::from(*number);
                } else if column == "agent_id" {
                    key.1 = i64::from(*number);
                }
            }
            _ => {
                return Err(invalid(format!(
                    "shared snapshot has invalid {}.{column}; refusing lossy copy",
                    table.name
                )));
            }
        }
        values.push(value.clone());
    }
    Ok((values, key))
}

/// The parent owns one source read transaction and publishes only on success.
/// Each page holds at most 1000 compact rows; the composite cursor can resume
/// inside a single delivery's receipt population. No source write is performed.
pub(super) fn transfer(src: &impl SnapshotSource, dst: &CanonicalDbConn) -> Result<(), ShareError> {
    if !source_has_shared_schema(src)? {
        return Ok(());
    }
    for table in TABLES {
        create_dst_table(dst, table)?;
        let columns = super::quoted_column_list(table.columns);
        // Never replace an already-copied receipt: a duplicate is evidence of
        // invalid source identity, not authority to discard an acknowledgement.
        let insert = format!(
            "INSERT INTO \"{}\" ({columns}) VALUES (?1, ?2, ?3, ?4)",
            table.name
        );
        let mut after = None;
        loop {
            let (sql, params) = page_query(table, after);
            let rows = query(src, &sql, &params)?;
            if rows.len() > PAGE_SIZE {
                return Err(invalid("shared snapshot page exceeded its row bound"));
            }
            if rows.is_empty() {
                break;
            }
            dst.execute_raw("BEGIN IMMEDIATE")
                .map_err(|error| ShareError::Sqlite {
                    message: format!("begin shared snapshot page: {error}"),
                })?;
            for row in &rows {
                let (values, key) = values_and_key(table, row)?;
                if after.is_some_and(|previous| key <= previous) {
                    return Err(invalid("shared snapshot cursor did not advance"));
                }
                dst.execute_sync(&insert, &values)
                    .map_err(|error| ShareError::Sqlite {
                        message: format!("copy shared snapshot table {}: {error}", table.name),
                    })?;
                after = Some(key);
            }
            dst.execute_raw("COMMIT")
                .map_err(|error| ShareError::Sqlite {
                    message: format!("commit shared snapshot page: {error}"),
                })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScrubPreset;
    use crate::snapshot::{
        DbConn, SnapshotPurpose, create_private_canonical_snapshot_context,
        create_private_canonical_sqlite_snapshot, create_sqlite_snapshot,
        transfer_tables_in_consistent_read,
    };
    use std::cell::{Cell, RefCell};
    use std::path::Path;

    fn seed(conn: &impl SnapshotSource) {
        for sql in [
            "CREATE TABLE projects(id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
            "CREATE TABLE agents(id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT DEFAULT 'test', model TEXT DEFAULT 'test', task_description TEXT DEFAULT '', inception_ts INTEGER DEFAULT 0, last_active_ts INTEGER DEFAULT 0, attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto', registration_token TEXT DEFAULT 'private-registration-token')",
            "CREATE TABLE messages(id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, thread_id TEXT DEFAULT 'thread', topic TEXT DEFAULT 'handoff', subject TEXT DEFAULT 'subject', body_md TEXT DEFAULT 'body', importance TEXT DEFAULT 'high', ack_required INTEGER DEFAULT 1, created_ts INTEGER DEFAULT 100, recipients_json TEXT DEFAULT '{}', attachments TEXT DEFAULT '[]')",
            "CREATE TABLE message_recipients(message_id INTEGER, agent_id INTEGER, kind TEXT, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id,agent_id))",
            "CREATE TABLE project_mailbox_deliveries(message_id INTEGER PRIMARY KEY REFERENCES messages(id), project_id INTEGER NOT NULL REFERENCES projects(id), kind TEXT NOT NULL, delivered_ts INTEGER NOT NULL)",
            "CREATE TABLE project_mailbox_receipts(message_id INTEGER NOT NULL REFERENCES messages(id), agent_id INTEGER NOT NULL REFERENCES agents(id), read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id,agent_id))",
            "INSERT INTO projects VALUES(1,'one','/one',0),(2,'two','/two',0)",
            "INSERT INTO agents(id,project_id,name) VALUES(11,1,'GreenStone'),(12,1,'BlueLake'),(13,1,'RedFox'),(21,2,'GoldFox'),(22,2,'BlueLake')",
            "INSERT INTO messages(id,project_id,sender_id,recipients_json) VALUES(101,1,11,'{\"to\":[\"project:one\"],\"cc\":[],\"bcc\":[]}'),(102,1,11,'{\"to\":[],\"cc\":[\"project:one\"],\"bcc\":[\"BlueLake\"]}'),(201,2,21,'{\"to\":[\"project:two\"],\"cc\":[],\"bcc\":[]}')",
            "INSERT INTO message_recipients VALUES(102,12,'bcc',17,19)",
            "INSERT INTO project_mailbox_deliveries VALUES(101,1,'to',1000),(102,1,'cc',2000),(201,2,'to',3000)",
            "INSERT INTO project_mailbox_receipts VALUES(101,12,111,112),(101,13,121,NULL),(102,12,NULL,NULL),(201,22,211,212)",
        ] {
            conn.execute_snapshot(sql).unwrap();
        }
    }

    type Delivery = (i64, i64, String, i64);
    type Receipt = (i64, i64, Option<i64>, Option<i64>);

    fn deliveries(conn: &CanonicalDbConn) -> Vec<Delivery> {
        conn.query_sync("SELECT message_id,project_id,kind,delivered_ts FROM project_mailbox_deliveries ORDER BY message_id", &[])
            .unwrap().iter().map(|row| (
                row.get_named("message_id").unwrap(), row.get_named("project_id").unwrap(),
                row.get_named("kind").unwrap(), row.get_named("delivered_ts").unwrap(),
            )).collect()
    }

    fn receipts(conn: &CanonicalDbConn) -> Vec<Receipt> {
        conn.query_sync("SELECT message_id,agent_id,read_ts,ack_ts FROM project_mailbox_receipts ORDER BY message_id,agent_id", &[])
            .unwrap().iter().map(|row| (
                row.get_named("message_id").unwrap(), row.get_named("agent_id").unwrap(),
                row.get_named("read_ts").unwrap(), row.get_named("ack_ts").unwrap(),
            )).collect()
    }

    fn scalar(conn: &CanonicalDbConn, sql: &str) -> i64 {
        conn.query_sync(sql, &[]).unwrap()[0].get_as(0).unwrap()
    }

    fn open(path: &Path) -> CanonicalDbConn {
        CanonicalDbConn::open_file(path.to_str().unwrap()).unwrap()
    }

    fn assert_original_shared(conn: &CanonicalDbConn) {
        assert_eq!(
            deliveries(conn),
            vec![
                (101, 1, "to".into(), 1000),
                (102, 1, "cc".into(), 2000),
                (201, 2, "to".into(), 3000)
            ]
        );
        assert_eq!(
            receipts(conn),
            vec![
                (101, 12, Some(111), Some(112)),
                (101, 13, Some(121), None),
                (102, 12, None, None),
                (201, 22, Some(211), Some(212))
            ]
        );
        assert_eq!(scalar(conn, "SELECT COUNT(*) FROM message_recipients"), 1);
        assert_eq!(
            scalar(
                conn,
                "SELECT COUNT(*) FROM project_mailbox_receipts WHERE typeof(ack_ts)='text' OR typeof(read_ts)='text'"
            ),
            0
        );
    }

    #[test]
    fn live_and_private_snapshots_preserve_shared_state_across_rebuilds() {
        for runtime in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.sqlite3");
            let copy = directory.path().join("copy.sqlite3");
            let rebuilt = directory.path().join("rebuilt.sqlite3");
            if runtime {
                let source_conn = DbConn::open_file(source.to_str().unwrap()).unwrap();
                seed(&source_conn);
                drop(source_conn);
                create_sqlite_snapshot(&source, &copy, false).unwrap();
            } else {
                seed(&open(&source));
                create_private_canonical_sqlite_snapshot(&source, &copy, false).unwrap();
            }
            assert_original_shared(&open(&copy));
            create_private_canonical_sqlite_snapshot(&copy, &rebuilt, false).unwrap();
            assert_original_shared(&open(&rebuilt));
        }
    }

    #[test]
    fn full_context_preserves_routing_and_applies_each_presets_privacy_policy() {
        for (preset, purpose) in [
            (ScrubPreset::Standard, SnapshotPurpose::Publish),
            (ScrubPreset::Strict, SnapshotPurpose::Publish),
            (ScrubPreset::Archive, SnapshotPurpose::Publish),
            (ScrubPreset::Archive, SnapshotPurpose::PrivateArchive),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.sqlite3");
            let copy = directory.path().join("copy.sqlite3");
            seed(&open(&source));
            let context = create_private_canonical_snapshot_context(
                &source,
                &copy,
                &["one".into()],
                preset,
                purpose,
            )
            .unwrap();
            assert_eq!(context.scope.projects.len(), 1);
            assert_eq!(context.scope.projects[0].slug, "one");
            let copied = open(&copy);
            let strict = matches!(preset, ScrubPreset::Strict);
            let archive = matches!(preset, ScrubPreset::Archive);
            assert_eq!(scalar(&copied, "SELECT COUNT(*) FROM messages"), 2);
            assert_eq!(deliveries(&copied).len(), if strict { 0 } else { 2 });
            if archive {
                assert_eq!(
                    receipts(&copied),
                    vec![
                        (101, 12, Some(111), Some(112)),
                        (101, 13, Some(121), None),
                        (102, 12, None, None)
                    ]
                );
            } else {
                assert!(receipts(&copied).is_empty());
            }
            let value: String = copied
                .query_sync("SELECT recipients_json FROM messages WHERE id=101", &[])
                .unwrap()[0]
                .get_named("recipients_json")
                .unwrap();
            let routing: serde_json::Value = serde_json::from_str(&value).unwrap();
            let expected_to: Vec<&str> = if strict { vec![] } else { vec!["project:one"] };
            assert_eq!(
                routing,
                serde_json::json!({"to":expected_to,"cc":[],"bcc":[]})
            );
            assert_eq!(
                scalar(
                    &copied,
                    "SELECT COUNT(*) FROM agents WHERE registration_token IS NOT NULL"
                ),
                if purpose == SnapshotPurpose::PrivateArchive {
                    3
                } else {
                    0
                }
            );
            assert_original_shared(&open(&source));
        }
    }

    #[test]
    fn incomplete_or_malformed_shared_schema_never_publishes_a_lossy_snapshot() {
        for damage in [
            "ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts",
            "ALTER TABLE project_mailbox_deliveries RENAME TO retained_deliveries",
            "ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts; CREATE VIEW project_mailbox_receipts AS SELECT * FROM retained_receipts",
            "ALTER TABLE project_mailbox_receipts RENAME COLUMN ack_ts TO retained_ack_ts",
            "ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts; CREATE TABLE project_mailbox_receipts AS SELECT * FROM retained_receipts",
            "UPDATE project_mailbox_receipts SET ack_ts='not a timestamp' WHERE message_id=101 AND agent_id=12",
            "UPDATE project_mailbox_deliveries SET kind='bcc' WHERE message_id=101",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.sqlite3");
            let copy = directory.path().join("copy.sqlite3");
            let conn = open(&source);
            seed(&conn);
            conn.execute_raw(damage).unwrap();
            drop(conn);
            let bytes = std::fs::read(&source).unwrap();
            assert!(
                create_private_canonical_sqlite_snapshot(&source, &copy, false).is_err(),
                "{damage}"
            );
            assert!(!copy.exists(), "{damage}");
            assert_eq!(std::fs::read(&source).unwrap(), bytes, "{damage}");
        }
    }

    struct RecordingSource<'a> {
        conn: &'a CanonicalDbConn,
        pages: RefCell<Vec<usize>>,
    }

    impl SnapshotSource for RecordingSource<'_> {
        fn execute_snapshot(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
            self.conn.execute_raw(sql)
        }

        fn query_snapshot(
            &self,
            sql: &str,
            params: &[Value],
        ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
            let rows = self.conn.query_sync(sql, params)?;
            if sql.contains("FROM \"project_mailbox_") {
                assert!(sql.ends_with("LIMIT 1000"));
                assert!(params.len() <= 2);
                self.pages.borrow_mut().push(rows.len());
            }
            Ok(rows)
        }
    }

    #[test]
    fn composite_receipt_pages_resume_inside_a_single_message_population() {
        let directory = tempfile::tempdir().unwrap();
        let source = open(&directory.path().join("source.sqlite3"));
        seed(&source);
        source.execute_raw("BEGIN IMMEDIATE").unwrap();
        for id in 1000..3005 {
            source
                .execute_sync(
                    "INSERT INTO agents(id,project_id,name) VALUES(?1,1,?2)",
                    &[Value::BigInt(id), Value::Text(format!("Viewer{id}"))],
                )
                .unwrap();
            source
                .execute_sync(
                    "INSERT INTO project_mailbox_receipts VALUES(101,?1,?2,?3)",
                    &[
                        Value::BigInt(id),
                        Value::BigInt(id * 2),
                        if id % 2 == 0 {
                            Value::BigInt(id * 3)
                        } else {
                            Value::Null
                        },
                    ],
                )
                .unwrap();
        }
        source.execute_raw("COMMIT").unwrap();
        let expected = receipts(&source);
        let recorded = RecordingSource {
            conn: &source,
            pages: RefCell::new(Vec::new()),
        };
        let destination = open(&directory.path().join("destination.sqlite3"));
        transfer_tables_in_consistent_read(&recorded, &destination).unwrap();
        assert_eq!(receipts(&destination), expected);
        assert_eq!(*recorded.pages.borrow(), [3, 0, 1000, 1000, 9, 0]);
        assert_eq!(
            scalar(&destination, "SELECT COUNT(*) FROM message_recipients"),
            1
        );
    }

    #[test]
    fn receipt_continuation_seeks_past_previous_message_populations() {
        let directory = tempfile::tempdir().unwrap();
        let source = open(&directory.path().join("receipt-seek.sqlite3"));
        seed(&source);
        let table = &TABLES[1];
        let (sql, params) = page_query(table, Some((101, 12)));
        let plan = source
            .query_sync(&format!("EXPLAIN QUERY PLAN {sql}"), &params)
            .unwrap();
        assert!(
            plan.iter().any(|row| {
                let detail = row.get_named::<String>("detail").unwrap();
                detail.contains("SEARCH") && detail.contains("project_mailbox_receipts")
            }),
            "continuation must seek into the real primary-key index: {plan:?}"
        );
        let rows = source.query_sync(&sql, &params).unwrap();
        let keys = rows
            .iter()
            .map(|row| {
                (
                    row.get_named::<i64>("message_id").unwrap(),
                    row.get_named::<i64>("agent_id").unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(keys, [(101, 13), (102, 12), (201, 22)]);
    }

    #[test]
    fn shared_receipts_and_messages_observe_one_real_wal_snapshot() {
        struct RacingSource<'a> {
            reader: &'a CanonicalDbConn,
            writer: &'a CanonicalDbConn,
            fired: Cell<bool>,
        }
        impl SnapshotSource for RacingSource<'_> {
            fn execute_snapshot(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.reader.execute_raw(sql)
            }
            fn query_snapshot(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
                let rows = self.reader.query_sync(sql, params)?;
                if sql.contains("FROM \"projects\"") && !self.fired.replace(true) {
                    self.writer.execute_raw("BEGIN IMMEDIATE; UPDATE messages SET subject='peer changed subject' WHERE id=101; UPDATE project_mailbox_receipts SET ack_ts=999 WHERE message_id=101 AND agent_id=12; COMMIT")?;
                }
                Ok(rows)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.sqlite3");
        let writer = open(&path);
        writer.execute_raw("PRAGMA journal_mode=WAL").unwrap();
        seed(&writer);
        let reader = open(&path);
        let source = RacingSource {
            reader: &reader,
            writer: &writer,
            fired: Cell::new(false),
        };
        let destination = open(&directory.path().join("destination.sqlite3"));
        transfer_tables_in_consistent_read(&source, &destination).unwrap();
        assert!(source.fired.get());
        assert_original_shared(&destination);
        assert_eq!(
            scalar(
                &writer,
                "SELECT ack_ts FROM project_mailbox_receipts WHERE message_id=101 AND agent_id=12"
            ),
            999
        );
        assert_eq!(
            destination
                .query_sync("SELECT subject FROM messages WHERE id=101", &[])
                .unwrap()[0]
                .get_named::<String>("subject")
                .unwrap(),
            "subject"
        );
    }

    #[test]
    fn pre_mailbox_sources_remain_supported_without_inventing_deliveries() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite3");
        let copy = directory.path().join("copy.sqlite3");
        open(&source).execute_raw("CREATE TABLE projects(id INTEGER PRIMARY KEY,slug TEXT,human_key TEXT,created_at INTEGER); INSERT INTO projects VALUES(1,'old','/old',0)").unwrap();
        create_private_canonical_sqlite_snapshot(&source, &copy, false).unwrap();
        let copied = open(&copy);
        assert_eq!(scalar(&copied, "SELECT COUNT(*) FROM projects"), 1);
        assert_eq!(
            scalar(
                &copied,
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('project_mailbox_deliveries','project_mailbox_receipts')"
            ),
            0
        );
    }

    const EVENT_TABLE: &str = "CREATE TABLE inbox_delivery_events(seq INTEGER PRIMARY KEY, project_id INTEGER, \
         agent_id INTEGER, message_id INTEGER, kind TEXT, delivered_ts INTEGER)";

    fn retain_event_after_shared_table_loss(conn: &impl SnapshotSource, historical: bool) {
        seed(conn);
        conn.execute_snapshot(EVENT_TABLE).unwrap();
        let recipient = if historical { "0" } else { "NULL" };
        conn.execute_snapshot(&format!(
            "INSERT INTO inbox_delivery_events VALUES(1,2,{recipient},201,'project',3000)"
        ))
        .unwrap();
        conn.execute_snapshot("ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts")
            .unwrap();
        conn.execute_snapshot(
            "ALTER TABLE project_mailbox_deliveries RENAME TO retained_deliveries",
        )
        .unwrap();
    }

    #[test]
    fn lost_shared_tables_never_publish_live_or_private_snapshots() {
        for runtime in [false, true] {
            for historical in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let source = directory.path().join("source.sqlite3");
                let destination = directory.path().join("refused.sqlite3");
                if runtime {
                    let conn = DbConn::open_file(source.to_str().unwrap()).unwrap();
                    retain_event_after_shared_table_loss(&conn, historical);
                    drop(conn);
                } else {
                    retain_event_after_shared_table_loss(&open(&source), historical);
                }
                let before = std::fs::read(&source).unwrap();
                let result = if runtime {
                    create_sqlite_snapshot(&source, &destination, false)
                } else {
                    create_private_canonical_sqlite_snapshot(&source, &destination, false)
                };
                let error = result.unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("retained project delivery events")
                );
                assert!(!destination.exists());
                assert_eq!(std::fs::read(&source).unwrap(), before);
            }
        }
    }

    #[test]
    fn legacy_event_ledgers_without_shared_deliveries_still_copy() {
        for direct_event in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("legacy.sqlite3");
            let destination = directory.path().join("copy.sqlite3");
            let conn = open(&source);
            conn.execute_raw(
                "CREATE TABLE projects(id INTEGER PRIMARY KEY,slug TEXT,human_key TEXT,created_at INTEGER); \
                 INSERT INTO projects VALUES(1,'old','/old',0)",
            )
            .unwrap();
            conn.execute_raw(EVENT_TABLE).unwrap();
            if direct_event {
                conn.execute_raw("INSERT INTO inbox_delivery_events VALUES(1,1,11,101,'to',10)")
                    .unwrap();
            }
            drop(conn);
            create_private_canonical_sqlite_snapshot(&source, &destination, false).unwrap();
            let copied = open(&destination);
            assert_eq!(scalar(&copied, "SELECT COUNT(*) FROM projects"), 1);
            assert_eq!(
                scalar(
                    &copied,
                    "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'project_mailbox_%'"
                ),
                0,
            );
        }
    }

    #[test]
    fn unavailable_event_authority_cannot_publish_a_legacy_snapshot() {
        for definition in [
            "CREATE TABLE inbox_delivery_events(seq INTEGER PRIMARY KEY)",
            "CREATE TABLE inbox_delivery_events(kind TEXT)",
            "CREATE VIEW inbox_delivery_events AS SELECT seq,kind FROM unavailable_events",
            "CREATE VIEW inbox_delivery_events AS SELECT 1 AS seq,'to' AS kind",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.sqlite3");
            let destination = directory.path().join("refused.sqlite3");
            open(&source).execute_raw(definition).unwrap();
            let before = std::fs::read(&source).unwrap();
            assert!(
                create_private_canonical_sqlite_snapshot(&source, &destination, false).is_err(),
                "{definition}",
            );
            assert!(!destination.exists());
            assert_eq!(std::fs::read(&source).unwrap(), before);
        }
    }

    #[test]
    fn complete_shared_snapshot_does_not_consult_unrelated_event_objects() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite3");
        let destination = directory.path().join("copy.sqlite3");
        let conn = open(&source);
        seed(&conn);
        conn.execute_raw(
            "CREATE VIEW inbox_delivery_events AS SELECT seq,kind FROM unavailable_events",
        )
        .unwrap();
        drop(conn);
        create_private_canonical_sqlite_snapshot(&source, &destination, false).unwrap();
        assert_original_shared(&open(&destination));
    }

    #[test]
    fn legacy_event_proof_uses_the_same_wal_snapshot_as_the_table_catalog() {
        struct EventRace<'a> {
            reader: &'a CanonicalDbConn,
            writer: &'a CanonicalDbConn,
            fired: Cell<bool>,
        }
        impl SnapshotSource for EventRace<'_> {
            fn execute_snapshot(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.reader.execute_raw(sql)
            }

            fn query_snapshot(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
                let rows = self.reader.query_sync(sql, params)?;
                if sql.starts_with("SELECT name, type FROM sqlite_master")
                    && !self.fired.replace(true)
                {
                    self.writer.execute_raw(
                        "INSERT INTO inbox_delivery_events VALUES(1,2,NULL,201,'project',3000)",
                    )?;
                }
                Ok(rows)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.sqlite3");
        let writer = open(&path);
        writer.execute_raw("PRAGMA journal_mode=WAL").unwrap();
        writer.execute_raw(EVENT_TABLE).unwrap();
        let reader = open(&path);
        let source = EventRace {
            reader: &reader,
            writer: &writer,
            fired: Cell::new(false),
        };
        reader.execute_raw("BEGIN").unwrap();
        assert!(!source_has_shared_schema(&source).unwrap());
        assert!(source.fired.get());
        assert_eq!(
            scalar(&writer, "SELECT COUNT(*) FROM inbox_delivery_events"),
            1
        );
        reader.execute_raw("COMMIT").unwrap();
        assert!(source_has_shared_schema(&reader).is_err());
    }
}
