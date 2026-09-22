//! Transactional admission for automatic overdue-ACK reservation grants.
//!
//! A scan row is a proposal, not permission to mutate. Recheck its exact
//! delivery, project, identities, generation and active conflicts in the same
//! immediate transaction as the grant. An ACK committed before admission makes
//! the proposal a no-op. A later ACK does not retroactively revoke a valid grant.

use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::pattern_overlap::CompiledPattern;
use sqlmodel_core::{Connection as _, IsolationLevel, Row, TransactionOps, Value};
use sqlmodel_pool::PooledConnection;

use crate::queries::{ACTIVE_RESERVATION_PREDICATE, UnackedMessageRow};
use crate::{DbConn, DbError, DbPool, FileReservationRow};

const MAX_ACTIVE_CANDIDATES: usize = 4096;
const MAX_PATTERN_BYTES: usize = 4096;
const REASON: &str = "ack-overdue";

/// The identities used to render the grant must match the database at admission.
/// `generation_id == None` means an unstamped database, not a wildcard.
pub struct AckEscalationRequest<'a> {
    pub observed: &'a UnackedMessageRow,
    pub generation_id: Option<&'a str>,
    pub project_slug: &'a str,
    pub project_key: &'a str,
    pub recipient_name: &'a str,
    pub holder_id: i64,
    pub holder_name: &'a str,
    pub ttl_seconds: i64,
    pub exclusive: bool,
}

const AUTHORIZED_SQL: &str = "\
SELECT 1 AS authorized \
FROM messages m JOIN message_recipients mr ON mr.message_id = m.id \
JOIN agents recipient ON recipient.id = mr.agent_id \
JOIN projects p ON p.id = m.project_id \
JOIN agents holder ON holder.id = ?5 \
WHERE m.id = ?1 AND m.project_id = ?2 AND m.created_ts = ?3 \
  AND m.ack_required = 1 AND mr.agent_id = ?4 AND mr.ack_ts IS NULL \
  AND recipient.project_id = ?2 AND recipient.name = ?6 COLLATE BINARY \
  AND holder.project_id = ?2 AND holder.name = ?7 COLLATE BINARY \
  AND p.slug = ?8 COLLATE BINARY AND p.human_key = ?9 COLLATE BINARY \
  AND (SELECT generation_id FROM db_identity WHERE singleton = 0) IS ?10 \
LIMIT 1";

const INSERT_SQL: &str = "\
INSERT INTO file_reservations \
(project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) \
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)";

fn sql_error(error: sqlmodel_core::Error) -> DbError {
    DbError::Sqlite(error.to_string())
}

macro_rules! take_sql {
    ($value:expr) => {
        match $value {
            Outcome::Ok(value) => value,
            Outcome::Err(error) => return Outcome::Err(sql_error(error)),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
    };
}

fn literal_agent(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 1024
        && !matches!(name, "." | "..")
        && !name.chars().any(|character| {
            character.is_control()
                || matches!(character, '/' | '\\' | '*' | '?' | '[' | ']' | '{' | '}')
        })
}

fn claim_pattern(request: &AckEscalationRequest<'_>) -> Result<String, DbError> {
    if request.observed.message_id <= 0
        || request.observed.project_id <= 0
        || request.observed.agent_id <= 0
        || request.holder_id <= 0
        || request.ttl_seconds <= 0
        || !literal_agent(request.recipient_name)
        || !literal_agent(request.holder_name)
        || request.project_slug.is_empty()
        || request.project_slug.len() > 1024
        || request.project_key.is_empty()
        || request.project_key.len() > 8192
        || request.generation_id.is_some_and(|value| value.is_empty() || value.len() > 256)
    {
        return Err(DbError::invalid("ack_escalation", "invalid observed claim identity or TTL"));
    }
    let created = chrono::DateTime::from_timestamp_micros(request.observed.created_ts)
        .ok_or_else(|| DbError::invalid("created_ts", "ACK month cannot be represented"))?;
    Ok(format!(
        "agents/{}/inbox/{}/*.md",
        request.recipient_name,
        created.format("%Y/%m")
    ))
}

async fn query(
    tx: &(impl TransactionOps + Sync),
    cx: &Cx,
    sql: &str,
    params: &[Value],
) -> Outcome<Vec<Row>, sqlmodel_core::Error> {
    let started = crate::tracking::query_timer();
    let result = tx.query(cx, sql, params).await;
    crate::tracking::record_query(sql, crate::tracking::elapsed_us(started));
    result
}

async fn apply_claim(
    tx: &(impl TransactionOps + Sync),
    cx: &Cx,
    request: &AckEscalationRequest<'_>,
    pattern: &str,
) -> Outcome<Option<FileReservationRow>, DbError> {
    let observed = request.observed;
    let authorized = take_sql!(query(
        tx,
        cx,
        AUTHORIZED_SQL,
        &[
            observed.message_id.into(),
            observed.project_id.into(),
            observed.created_ts.into(),
            observed.agent_id.into(),
            request.holder_id.into(),
            request.recipient_name.into(),
            request.holder_name.into(),
            request.project_slug.into(),
            request.project_key.into(),
            request.generation_id.map_or(Value::Null, |value| Value::Text(value.to_string())),
        ],
    ).await);
    if authorized.is_empty() {
        return Outcome::Ok(None);
    }

    let now = crate::now_micros();
    // Reuse the authoritative release-ledger predicate. Bound both the number
    // of candidates and each projected pattern; overflow is deferral, not proof
    // that an unexamined reservation does not conflict.
    let active_sql = format!(
        "SELECT agent_id, \
         CASE WHEN length(CAST(path_pattern AS BLOB)) <= {MAX_PATTERN_BYTES} \
              THEN path_pattern ELSE NULL END AS path_pattern, \
         \"exclusive\" AS is_exclusive, \
         CASE WHEN length(CAST(reason AS BLOB)) <= 256 THEN reason ELSE NULL END AS reason \
         FROM file_reservations WHERE project_id = ?1 \
           AND ({ACTIVE_RESERVATION_PREDICATE}) AND expires_ts > ?2 \
         ORDER BY id ASC LIMIT {}",
        MAX_ACTIVE_CANDIDATES + 1,
    );
    let active = take_sql!(query(tx, cx, &active_sql, &[observed.project_id.into(), now.into()]).await);
    if active.len() > MAX_ACTIVE_CANDIDATES {
        return Outcome::Err(DbError::ResourceBusy("ACK escalation conflict budget exceeded".into()));
    }
    let requested = CompiledPattern::cached(pattern);
    let mut already_claimed = false;
    for row in active {
        let holder = match row.get_named::<i64>("agent_id") {
            Ok(id) if id > 0 => id,
            _ => return Outcome::Err(DbError::Internal("invalid active reservation holder".into())),
        };
        let other_pattern = match row.get_named::<String>("path_pattern") {
            Ok(pattern) => pattern,
            Err(error) => return Outcome::Err(sql_error(error)),
        };
        let exclusive = match row.get_named::<i64>("is_exclusive") {
            Ok(value @ (0 | 1)) => value != 0,
            _ => return Outcome::Err(DbError::Internal("invalid active reservation exclusivity".into())),
        };
        if holder == request.holder_id {
            if other_pattern == pattern {
                let reason = match row.get_named::<String>("reason") {
                    Ok(reason) => reason,
                    Err(error) => return Outcome::Err(sql_error(error)),
                };
                if reason != REASON || exclusive != request.exclusive {
                    return Outcome::Err(DbError::ResourceBusy(
                        "ACK escalation refuses to renew an unrelated same-path lease".into(),
                    ));
                }
                already_claimed = true;
            }
        } else if (request.exclusive || exclusive)
            && CompiledPattern::cached(&other_pattern).overlaps(requested.as_ref())
        {
            return Outcome::Err(DbError::ResourceBusy("ACK escalation reservation conflict".into()));
        }
    }
    if already_claimed {
        // Repeated scans must not indefinitely extend a matching active lease.
        return Outcome::Ok(None);
    }

    let mut reservation = FileReservationRow {
        id: None,
        project_id: observed.project_id,
        agent_id: request.holder_id,
        path_pattern: pattern.to_string(),
        exclusive: i64::from(request.exclusive),
        reason: REASON.to_string(),
        created_ts: now,
        expires_ts: now.saturating_add(request.ttl_seconds.saturating_mul(1_000_000)),
        released_ts: None,
    };
    let params = [
        reservation.project_id.into(), reservation.agent_id.into(),
        Value::Text(reservation.path_pattern.clone()), reservation.exclusive.into(),
        Value::Text(reservation.reason.clone()), reservation.created_ts.into(),
        reservation.expires_ts.into(),
    ];
    let started = crate::tracking::query_timer();
    let inserted = tx.execute(cx, INSERT_SQL, &params).await;
    crate::tracking::record_query(INSERT_SQL, crate::tracking::elapsed_us(started));
    take_sql!(inserted);
    let rows = take_sql!(query(tx, cx, "SELECT last_insert_rowid() AS id", &[]).await);
    let id = match rows.first().and_then(|row| row.get_named::<i64>("id").ok()) {
        Some(id) if id > 0 => id,
        _ => return Outcome::Err(DbError::Internal("ACK grant has no valid inserted row ID".into())),
    };
    reservation.id = Some(id);
    Outcome::Ok(Some(reservation))
}

async fn grant_on_connection(
    cx: &Cx,
    conn: &DbConn,
    request: &AckEscalationRequest<'_>,
    pattern: &str,
) -> Outcome<Option<FileReservationRow>, DbError> {
    // SQLModel's SQLite/FrankenSQLite ReadCommitted mode maps to BEGIN IMMEDIATE.
    // Do not nest the ordinary pool-acquiring reservation writer inside this tx.
    let tx = take_sql!(conn.begin_with(cx, IsolationLevel::ReadCommitted).await);
    let result = apply_claim(&tx, cx, request, pattern).await;
    match result {
        Outcome::Ok(Some(reservation)) => {
            let started = crate::tracking::query_timer();
            let committed = tx.commit(cx).await;
            mcp_agent_mail_core::metrics::record_database_write_latency(
                crate::tracking::elapsed_us(started),
            );
            take_sql!(committed);
            Outcome::Ok(Some(reservation))
        }
        Outcome::Ok(None) => {
            take_sql!(tx.rollback(cx).await);
            Outcome::Ok(None)
        }
        other => {
            // The native transaction rolls back on drop, also during unwinding.
            drop(tx);
            other
        }
    }
}

/// A failed/cancelled/unwound transaction must not return an uncertain session
/// to the pool. Native transaction cleanup runs before this owning lease drops.
struct ClaimLease {
    connection: Option<PooledConnection<DbConn>>,
    reusable: bool,
}

impl Drop for ClaimLease {
    fn drop(&mut self) {
        if !self.reusable && let Some(connection) = self.connection.take() {
            crate::close_db_conn(connection.detach(), "failed ACK escalation transaction");
        }
    }
}

/// Create one automatic grant only while its exact delivery remains eligible.
/// `None` means stale observation or an already-active matching claim. Neither
/// creates/renews a lease or authorizes another archive write. No local retry
/// loop is added: transient errors are retried by the worker's next scan lap.
/// The caller retains the existing generation/write lease through publication.
pub async fn grant_ack_escalation(
    cx: &Cx,
    pool: &DbPool,
    request: &AckEscalationRequest<'_>,
) -> Outcome<Option<FileReservationRow>, DbError> {
    let pattern = match claim_pattern(request) {
        Ok(pattern) => pattern,
        Err(error) => return Outcome::Err(error),
    };
    let connection = take_sql!(pool.acquire(cx).await);
    let mut lease = ClaimLease { connection: Some(connection), reusable: false };
    let result = grant_on_connection(
        cx,
        lease.connection.as_ref().expect("live ACK claim connection"),
        request,
        &pattern,
    ).await;
    lease.reusable = matches!(&result, Outcome::Ok(_));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build().unwrap().block_on(future)
    }

    fn fixture(test: impl FnOnce(&DbPool, &Cx, &str, &UnackedMessageRow)) {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let pool = crate::create_pool(&crate::DbPoolConfig {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&temp.path().join("mail.sqlite3")),
                min_connections: 1,
                max_connections: 2,
                ..Default::default()
            }).unwrap();
            let cx = Cx::for_testing();
            let generation = match run(crate::queries::db_generation_id(&cx, &pool)) {
                Outcome::Ok(Some(generation)) => generation,
                other => panic!("seed generation: {other:?}"),
            };
            execute(&pool, &cx, "INSERT INTO projects(id, slug, human_key, created_at) VALUES(101, 'project', '/project', 1), (201, 'foreign', '/foreign', 1)");
            execute(&pool, &cx, "INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES(101, 101, 'RedFox', 'test', 'test', '', 1, 1), (102, 101, 'BlueBear', 'test', 'test', '', 1, 1)");
            execute(&pool, &cx, "INSERT INTO messages(id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) VALUES(901, 101, 101, 'Overdue', 'Body', 'normal', 1, 1000000, '{}', '[]')");
            execute(&pool, &cx, "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES(901, 102, 'to')");
            let observed = UnackedMessageRow {
                message_id: 901, project_id: 101, created_ts: 1_000_000, agent_id: 102,
            };
            test(&pool, &cx, &generation, &observed);
        });
    }

    fn execute(pool: &DbPool, cx: &Cx, sql: &str) {
        let conn = match run(pool.acquire(cx)) {
            Outcome::Ok(conn) => conn,
            other => panic!("fixture checkout: {other:?}"),
        };
        conn.execute_raw(sql).unwrap();
    }

    fn scalar(pool: &DbPool, cx: &Cx, sql: &str) -> i64 {
        let conn = match run(pool.acquire(cx)) {
            Outcome::Ok(conn) => conn,
            other => panic!("fixture checkout: {other:?}"),
        };
        conn.query_sync(sql, &[]).unwrap()[0].get_as(0).unwrap()
    }

    fn request<'a>(generation: &'a str, observed: &'a UnackedMessageRow) -> AckEscalationRequest<'a> {
        AckEscalationRequest {
            observed, generation_id: Some(generation), project_slug: "project",
            project_key: "/project", recipient_name: "BlueBear", holder_id: 102,
            holder_name: "BlueBear", ttl_seconds: 3600, exclusive: true,
        }
    }

    fn granted(outcome: Outcome<Option<FileReservationRow>, DbError>) -> FileReservationRow {
        match outcome {
            Outcome::Ok(Some(row)) => row,
            other => panic!("expected committed grant: {other:?}"),
        }
    }

    fn stale(outcome: Outcome<Option<FileReservationRow>, DbError>) {
        assert!(matches!(outcome, Outcome::Ok(None)), "{outcome:?}");
    }

    #[test]
    fn grant_deduplicates_without_renewing_and_reacquires_after_release() {
        fixture(|pool, cx, generation, observed| {
            let request = request(generation, observed);
            let first = granted(run(grant_ack_escalation(cx, pool, &request)));
            assert_eq!(first.path_pattern, "agents/BlueBear/inbox/1970/01/*.md");
            stale(run(grant_ack_escalation(cx, pool, &request)));
            assert_eq!(scalar(pool, cx, "SELECT COUNT(*) FROM file_reservations"), 1);
            assert_eq!(scalar(pool, cx, "SELECT expires_ts FROM file_reservations"), first.expires_ts);
            match run(crate::queries::release_reservations(cx, pool, 101, 102, None, Some(&[first.id.unwrap()]))) {
                Outcome::Ok(rows) => assert_eq!(rows.len(), 1),
                other => panic!("release grant: {other:?}"),
            }
            let second = granted(run(grant_ack_escalation(cx, pool, &request)));
            assert_ne!(first.id, second.id);
            assert_eq!(scalar(pool, cx, "SELECT COUNT(*) FROM file_reservations"), 2);
        });
    }

    #[test]
    fn an_ack_committed_after_observation_prevents_the_grant() {
        fixture(|pool, cx, generation, observed| {
            execute(pool, cx, "UPDATE message_recipients SET ack_ts = 42 WHERE message_id = 901 AND agent_id = 102");
            stale(run(grant_ack_escalation(cx, pool, &request(generation, observed))));
            assert_eq!(scalar(pool, cx, "SELECT COUNT(*) FROM file_reservations"), 0);
            assert_eq!(scalar(pool, cx, "SELECT ack_ts FROM message_recipients"), 42);
        });
    }

    #[test]
    fn changed_delivery_identity_or_generation_never_authorizes_a_lease() {
        for sql in [
            "UPDATE messages SET created_ts = 2000000 WHERE id = 901",
            "UPDATE messages SET ack_required = 0 WHERE id = 901",
            "UPDATE messages SET project_id = 201 WHERE id = 901",
            "UPDATE message_recipients SET agent_id = 101 WHERE message_id = 901",
            "UPDATE agents SET name = 'bluebear' WHERE id = 102",
            "UPDATE agents SET project_id = 201 WHERE id = 102",
            "UPDATE projects SET slug = 'renamed' WHERE id = 101",
            "UPDATE projects SET human_key = '/changed' WHERE id = 101",
            "UPDATE db_identity SET generation_id = 'replacement-generation' WHERE singleton = 0",
        ] {
            fixture(|pool, cx, generation, observed| {
                execute(pool, cx, sql);
                stale(run(grant_ack_escalation(cx, pool, &request(generation, observed))));
                assert_eq!(scalar(pool, cx, "SELECT COUNT(*) FROM file_reservations"), 0, "{sql}");
            });
        }
    }

    #[test]
    fn custom_holder_is_revalidated_inside_the_grant_transaction() {
        fixture(|pool, cx, generation, observed| {
            let mut request = request(generation, observed);
            request.holder_id = 101;
            request.holder_name = "RedFox";
            execute(pool, cx, "UPDATE agents SET name = 'ChangedFox' WHERE id = 101");
            stale(run(grant_ack_escalation(cx, pool, &request)));
            assert_eq!(scalar(pool, cx, "SELECT COUNT(*) FROM file_reservations"), 0);
        });
    }

    #[test]
    fn unrelated_same_holder_lease_is_not_renewed_by_automatic_escalation() {
        fixture(|pool, cx, generation, observed| {
            let pattern = "agents/BlueBear/inbox/1970/01/*.md";
            match run(crate::queries::create_file_reservations(cx, pool, 101, 102, &[pattern], 60, true, "manual work")) {
                Outcome::Ok(rows) => assert_eq!(rows.len(), 1),
                other => panic!("manual lease: {other:?}"),
            }
            let expires = scalar(pool, cx, "SELECT expires_ts FROM file_reservations");
            assert!(matches!(run(grant_ack_escalation(cx, pool, &request(generation, observed))), Outcome::Err(DbError::ResourceBusy(_))));
            assert_eq!(scalar(pool, cx, "SELECT expires_ts FROM file_reservations"), expires);
            assert_eq!(scalar(pool, cx, "SELECT COUNT(*) FROM file_reservations"), 1);
        });
    }

    #[test]
    fn overlap_conflicts_obey_shared_and_exclusive_semantics() {
        for other_exclusive in [false, true] {
            for requested_exclusive in [false, true] {
                fixture(|pool, cx, generation, observed| {
                    match run(crate::queries::create_file_reservations(cx, pool, 101, 101, &["agents/BlueBear/inbox/**"], 3600, other_exclusive, "other work")) {
                        Outcome::Ok(rows) => assert_eq!(rows.len(), 1),
                        other => panic!("other holder: {other:?}"),
                    }
                    let mut request = request(generation, observed);
                    request.exclusive = requested_exclusive;
                    let result = run(grant_ack_escalation(cx, pool, &request));
                    if other_exclusive || requested_exclusive {
                        assert!(matches!(result, Outcome::Err(DbError::ResourceBusy(_))), "{result:?}");
                        assert_eq!(scalar(pool, cx, "SELECT COUNT(*) FROM file_reservations"), 1);
                    } else {
                        granted(result);
                        assert_eq!(scalar(pool, cx, "SELECT COUNT(*) FROM file_reservations"), 2);
                    }
                });
            }
        }
    }

    #[test]
    fn failed_transaction_releases_its_writer_and_preserves_rows() {
        fixture(|pool, cx, generation, observed| {
            let conn = match run(pool.acquire(cx)) {
                Outcome::Ok(conn) => conn,
                other => panic!("checkout: {other:?}"),
            };
            conn.execute_raw("ALTER TABLE file_reservations RENAME TO unavailable_claims").unwrap();
            let request = request(generation, observed);
            assert!(matches!(run(grant_on_connection(cx, &conn, &request, &claim_pattern(&request).unwrap())), Outcome::Err(_)));
            // A failed body must roll back before the connection can be reused.
            conn.execute_raw("BEGIN IMMEDIATE").unwrap();
            conn.execute_raw("ROLLBACK").unwrap();
            assert_eq!(conn.query_sync("SELECT COUNT(*) FROM unavailable_claims", &[]).unwrap()[0].get_as::<i64>(0).unwrap(), 0);
        });
    }

    #[test]
    fn unrepresentable_time_and_nonliteral_names_cannot_choose_an_inbox() {
        let mut observed = UnackedMessageRow { message_id: 1, project_id: 1, agent_id: 1, created_ts: -1 };
        let valid = request("generation", &observed);
        assert_eq!(claim_pattern(&valid).unwrap(), "agents/BlueBear/inbox/1969/12/*.md");
        for name in ["*", "../outside", "Ops[AB]", "a\\b", "a\n", ""] {
            let mut invalid = request("generation", &observed);
            invalid.recipient_name = name;
            assert!(claim_pattern(&invalid).is_err());
        }
        observed.created_ts = i64::MAX;
        assert!(claim_pattern(&request("generation", &observed)).is_err());
    }
}
