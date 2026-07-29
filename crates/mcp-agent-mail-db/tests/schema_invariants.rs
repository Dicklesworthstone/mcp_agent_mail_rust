//! Schema invariant regression and deterministic stateful coverage.
//!
//! The normal tests keep the sequence small and replayable.  Set
//! `AM_SCHEMA_INVARIANT_ARTIFACT_DIR=/tmp/am-schema-invariants` to persist
//! JSON traces with replay commands for each seed.

#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args
)]

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use asupersync::{Cx, Outcome};
use mcp_agent_mail_db::invariants::{
    SCHEMA_INVARIANT_REPLAY_COMMAND, SchemaInvariantKind, check_schema_invariants,
    check_schema_invariants_conn,
};
use mcp_agent_mail_db::{
    DbConn, DbError, DbPool, DbPoolConfig, open_sqlite_file_with_recovery, queries,
};
use serde_json::json;
use sqlmodel_core::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
struct MessageRef {
    recipient_id: i64,
    message_id: i64,
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_field_names)]
struct ReservationRef {
    project_id: i64,
    agent_id: i64,
    reservation_id: i64,
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_field_names)]
struct ContactRef {
    from_project_id: i64,
    from_agent_id: i64,
    to_project_id: i64,
    to_agent_id: i64,
}

#[derive(Debug)]
struct StatefulCorpus {
    projects: Vec<i64>,
    agents_by_project: Vec<Vec<i64>>,
    messages: Vec<MessageRef>,
    reservations: Vec<ReservationRef>,
    contacts: Vec<ContactRef>,
    products: Vec<i64>,
}

#[derive(Debug, Clone, Copy)]
struct Lcg(u64);

impl Lcg {
    const fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn index(&mut self, len: usize) -> usize {
        assert!(len > 0, "cannot select from empty collection");
        (self.next() as usize) % len
    }
}

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    common::block_on(f)
}

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("schema_invariants_{}.db", unique_suffix()));

    let init_conn = DbConn::open_file(db_path.display().to_string())
        .expect("open connection for schema invariant test pool");
    init_conn
        .execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_BASE_SQL)
        .expect("apply init PRAGMAs");
    let cx = Cx::for_testing();
    match common::spin_poll(mcp_agent_mail_db::schema::migrate_to_latest_base(
        &cx, &init_conn,
    )) {
        Outcome::Ok(_) => {}
        other => panic!("schema invariant test pool migration failed: {other:?}"),
    }
    drop(init_conn);

    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        storage_root: Some(db_path.parent().unwrap().join("storage")),
        max_connections: 5,
        min_connections: 1,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: false,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    };
    let pool = DbPool::new(&config).expect("create schema invariant test pool");
    (pool, dir)
}

fn fresh_conn(pool: &DbPool) -> DbConn {
    open_sqlite_file_with_recovery(pool.sqlite_path()).expect("open fresh sqlite connection")
}

fn allow_corruption_fixture(conn: &DbConn) {
    conn.execute_raw("PRAGMA foreign_keys = OFF")
        .expect("disable foreign keys for intentional corruption fixture");
}

fn expect_outcome<T>(outcome: Outcome<T, DbError>, context: &str) -> T {
    match outcome {
        Outcome::Ok(value) => value,
        Outcome::Err(error) => panic!("{context} failed: {error:?}"),
        Outcome::Cancelled(reason) => panic!("{context} cancelled: {reason:?}"),
        Outcome::Panicked(panic) => panic!("{context} panicked: {panic:?}"),
    }
}

fn setup_project(pool: &DbPool, seed: u64, idx: usize) -> i64 {
    let pool = pool.clone();
    block_on(|cx| async move {
        let key = format!("/tmp/schema-invariants/{seed}/{idx}/{}", unique_suffix());
        expect_outcome(
            queries::ensure_project(&cx, &pool, &key).await,
            "ensure_project",
        )
        .id
        .expect("project id")
    })
}

fn setup_agent(pool: &DbPool, project_id: i64, name: &str) -> i64 {
    let pool = pool.clone();
    let name = name.to_string();
    block_on(|cx| async move {
        expect_outcome(
            queries::register_agent(
                &cx,
                &pool,
                project_id,
                &name,
                "schema-invariant-test",
                "test-model",
                Some("schema invariant deterministic harness"),
                None,
                None,
            )
            .await,
            "register_agent",
        )
        .id
        .expect("agent id")
    })
}

fn send_msg(
    pool: &DbPool,
    project_id: i64,
    sender_id: i64,
    recipient_id: i64,
    subject: &str,
    thread_id: Option<&str>,
    ack_required: bool,
) -> i64 {
    let pool = pool.clone();
    let subject = subject.to_string();
    let thread_id = thread_id.map(String::from);
    block_on(|cx| async move {
        expect_outcome(
            queries::create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                &subject,
                "schema invariant body",
                thread_id.as_deref(),
                "normal",
                ack_required,
                "[]",
                &[(recipient_id, "to")],
            )
            .await,
            "create_message_with_recipients",
        )
        .id
        .expect("message id")
    })
}

fn mark_read(pool: &DbPool, recipient_id: i64, message_id: i64) {
    let pool = pool.clone();
    block_on(|cx| async move {
        expect_outcome(
            queries::mark_message_read(&cx, &pool, recipient_id, message_id).await,
            "mark_message_read",
        );
    });
}

fn acknowledge(pool: &DbPool, recipient_id: i64, message_id: i64) {
    let pool = pool.clone();
    block_on(|cx| async move {
        expect_outcome(
            queries::acknowledge_message(&cx, &pool, recipient_id, message_id).await,
            "acknowledge_message",
        );
    });
}

fn create_reservation(pool: &DbPool, project_id: i64, agent_id: i64, path: &str) -> i64 {
    let pool = pool.clone();
    let path = path.to_string();
    block_on(|cx| async move {
        let paths = [path.as_str()];
        let rows = expect_outcome(
            queries::create_file_reservations(
                &cx,
                &pool,
                project_id,
                agent_id,
                &paths,
                900,
                true,
                "schema invariant deterministic harness",
            )
            .await,
            "create_file_reservations",
        );
        rows.first().and_then(|row| row.id).expect("reservation id")
    })
}

fn release_reservation(pool: &DbPool, reservation: ReservationRef) {
    let pool = pool.clone();
    block_on(|cx| async move {
        let ids = [reservation.reservation_id];
        expect_outcome(
            queries::release_reservations(
                &cx,
                &pool,
                reservation.project_id,
                reservation.agent_id,
                None,
                Some(&ids),
            )
            .await,
            "release_reservations",
        );
    });
}

fn request_contact(pool: &DbPool, contact: ContactRef, step: usize) {
    let pool = pool.clone();
    block_on(|cx| async move {
        expect_outcome(
            queries::request_contact(
                &cx,
                &pool,
                contact.from_project_id,
                contact.from_agent_id,
                contact.to_project_id,
                contact.to_agent_id,
                &format!("schema invariant contact step {step}"),
                900,
            )
            .await,
            "request_contact",
        );
    });
}

fn respond_contact(pool: &DbPool, contact: ContactRef, accept: bool) {
    let pool = pool.clone();
    block_on(|cx| async move {
        expect_outcome(
            queries::respond_contact(
                &cx,
                &pool,
                contact.from_project_id,
                contact.from_agent_id,
                contact.to_project_id,
                contact.to_agent_id,
                accept,
                900,
            )
            .await,
            "respond_contact",
        );
    });
}

fn ensure_product_link(pool: &DbPool, uid: &str, project_id: i64) -> i64 {
    let pool = pool.clone();
    let uid = uid.to_string();
    block_on(|cx| async move {
        let product = expect_outcome(
            queries::ensure_product(&cx, &pool, Some(&uid), Some(&uid)).await,
            "ensure_product",
        );
        let product_id = product.id.expect("product id");
        expect_outcome(
            queries::link_product_to_projects(&cx, &pool, product_id, &[project_id]).await,
            "link_product_to_projects",
        );
        product_id
    })
}

fn invariant_failure(pool: &DbPool, context: &str) -> Option<String> {
    let conn = fresh_conn(pool);
    match check_schema_invariants_conn(&conn) {
        Ok(report) if report.is_healthy() => None,
        Ok(report) => Some(format!(
            "{context}: schema invariant findings={:?}; replay={}",
            report.findings, SCHEMA_INVARIANT_REPLAY_COMMAND
        )),
        Err(error) => Some(format!(
            "{context}: schema invariant check failed: {error:?}; replay={}",
            SCHEMA_INVARIANT_REPLAY_COMMAND
        )),
    }
}

fn assert_invariants_clean(pool: &DbPool, context: &str) {
    if let Some(failure) = invariant_failure(pool, context) {
        panic!("{failure}");
    }
}

fn write_sequence_artifact(seed: u64, steps: &[String], failure: Option<&str>) {
    let Ok(dir) = std::env::var("AM_SCHEMA_INVARIANT_ARTIFACT_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    fs::create_dir_all(&dir).expect("create schema invariant artifact dir");
    let path = dir.join(format!("schema-invariants-seed-{seed}.json"));
    let payload = json!({
        "seed": seed,
        "op_count": steps.len(),
        "replay_command": SCHEMA_INVARIANT_REPLAY_COMMAND,
        "steps": steps,
        "failure": failure,
    });
    let bytes = serde_json::to_vec_pretty(&payload).expect("serialize schema invariant artifact");
    fs::write(path, bytes).expect("write schema invariant artifact");
}

fn seed_stateful_corpus(pool: &DbPool, seed: u64) -> StatefulCorpus {
    let agent_names = ["RedFox", "BlueLake", "GoldHawk"];
    let mut projects = Vec::new();
    let mut agents_by_project = Vec::new();
    for project_idx in 0..2 {
        let project_id = setup_project(pool, seed, project_idx);
        let agents = agent_names
            .iter()
            .map(|name| setup_agent(pool, project_id, name))
            .collect();
        projects.push(project_id);
        agents_by_project.push(agents);
    }

    StatefulCorpus {
        projects,
        agents_by_project,
        messages: Vec::new(),
        reservations: Vec::new(),
        contacts: Vec::new(),
        products: Vec::new(),
    }
}

fn run_stateful_sequence(seed: u64, op_count: usize) {
    let (pool, _dir) = make_pool();
    let mut rng = Lcg(seed);
    let mut steps = Vec::new();
    let mut corpus = seed_stateful_corpus(&pool, seed);
    assert_invariants_clean(&pool, &format!("seed {seed} after corpus seed"));

    for step in 0..op_count {
        let op = rng.next() % 8;
        let description = match op {
            0 => {
                let project_idx = rng.index(corpus.projects.len());
                let agents = &corpus.agents_by_project[project_idx];
                let sender_idx = rng.index(agents.len());
                let mut recipient_idx = rng.index(agents.len());
                if recipient_idx == sender_idx {
                    recipient_idx = (recipient_idx + 1) % agents.len();
                }
                let subject = format!("schema invariant seed {seed} step {step}");
                let thread = format!("schema-thread-{seed}-{}", rng.next() % 5);
                let message_id = send_msg(
                    &pool,
                    corpus.projects[project_idx],
                    agents[sender_idx],
                    agents[recipient_idx],
                    &subject,
                    Some(&thread),
                    rng.next().is_multiple_of(2),
                );
                corpus.messages.push(MessageRef {
                    recipient_id: agents[recipient_idx],
                    message_id,
                });
                format!("send_msg project_idx={project_idx} message_id={message_id}")
            }
            1 => {
                if corpus.messages.is_empty() {
                    "mark_read skipped=no_messages".to_string()
                } else {
                    let message = corpus.messages[rng.index(corpus.messages.len())];
                    mark_read(&pool, message.recipient_id, message.message_id);
                    format!("mark_read message_id={}", message.message_id)
                }
            }
            2 => {
                if corpus.messages.is_empty() {
                    "acknowledge skipped=no_messages".to_string()
                } else {
                    let message = corpus.messages[rng.index(corpus.messages.len())];
                    acknowledge(&pool, message.recipient_id, message.message_id);
                    format!("acknowledge message_id={}", message.message_id)
                }
            }
            3 => {
                let project_idx = rng.index(corpus.projects.len());
                let agents = &corpus.agents_by_project[project_idx];
                let agent_id = agents[rng.index(agents.len())];
                let path = format!("src/schema_invariant_seed_{seed}_step_{step}.rs");
                let reservation_id =
                    create_reservation(&pool, corpus.projects[project_idx], agent_id, &path);
                corpus.reservations.push(ReservationRef {
                    project_id: corpus.projects[project_idx],
                    agent_id,
                    reservation_id,
                });
                format!("create_reservation reservation_id={reservation_id}")
            }
            4 => {
                if corpus.reservations.is_empty() {
                    "release_reservation skipped=no_reservations".to_string()
                } else {
                    let reservation = corpus.reservations[rng.index(corpus.reservations.len())];
                    release_reservation(&pool, reservation);
                    format!(
                        "release_reservation reservation_id={}",
                        reservation.reservation_id
                    )
                }
            }
            5 => {
                let from_project_idx = rng.index(corpus.projects.len());
                let to_project_idx = (from_project_idx + 1) % corpus.projects.len();
                let from_agents = &corpus.agents_by_project[from_project_idx];
                let to_agents = &corpus.agents_by_project[to_project_idx];
                let contact = ContactRef {
                    from_project_id: corpus.projects[from_project_idx],
                    from_agent_id: from_agents[rng.index(from_agents.len())],
                    to_project_id: corpus.projects[to_project_idx],
                    to_agent_id: to_agents[rng.index(to_agents.len())],
                };
                request_contact(&pool, contact, step);
                corpus.contacts.push(contact);
                format!(
                    "request_contact from_agent={} to_agent={}",
                    contact.from_agent_id, contact.to_agent_id
                )
            }
            6 => {
                if corpus.contacts.is_empty() {
                    "respond_contact skipped=no_contacts".to_string()
                } else {
                    let contact = corpus.contacts[rng.index(corpus.contacts.len())];
                    let accept = rng.next().is_multiple_of(2);
                    respond_contact(&pool, contact, accept);
                    format!("respond_contact accept={accept}")
                }
            }
            7 => {
                let project_idx = rng.index(corpus.projects.len());
                let uid = format!("schema-seed-{seed}-step-{step}-{}", rng.next() % 10_000);
                let product_id = ensure_product_link(&pool, &uid, corpus.projects[project_idx]);
                corpus.products.push(product_id);
                format!("ensure_product_link product_id={product_id}")
            }
            _ => unreachable!("op modulo covers all cases"),
        };
        steps.push(description);
        if let Some(failure) = invariant_failure(&pool, &format!("seed {seed} step {step}")) {
            write_sequence_artifact(seed, &steps, Some(&failure));
            panic!("{failure}");
        }
    }

    write_sequence_artifact(seed, &steps, None);
}

#[test]
fn schema_invariants_report_clean_database_is_healthy() {
    let (pool, _dir) = make_pool();
    let report = block_on(|cx| async move {
        expect_outcome(
            check_schema_invariants(&cx, &pool).await,
            "check_schema_invariants",
        )
    });

    assert!(report.is_healthy(), "clean database should be healthy");
    assert!(
        report
            .checked_scopes
            .iter()
            .any(|scope| scope == "build_slots:file_backed_external"),
        "DB invariant scope should explicitly mark file-backed build slots as external"
    );
    assert!(
        report.table_counts.contains_key("message_recipients"),
        "report should include message_recipient table counts"
    );
}

#[test]
fn schema_invariants_detect_orphaned_message_recipient_startup_repair_class() {
    let (pool, _dir) = make_pool();
    let project_id = setup_project(&pool, 100, 0);
    let sender_id = setup_agent(&pool, project_id, "RedFox");
    let recipient_id = setup_agent(&pool, project_id, "BlueLake");
    let orphaned_message_id = send_msg(
        &pool,
        project_id,
        sender_id,
        recipient_id,
        "orphaned parent regression",
        Some("orphaned-parent"),
        false,
    );
    let valid_message_id = send_msg(
        &pool,
        project_id,
        sender_id,
        recipient_id,
        "orphaned recipient regression",
        Some("orphaned-agent"),
        true,
    );

    let conn = fresh_conn(&pool);
    allow_corruption_fixture(&conn);
    conn.execute_raw("DROP TRIGGER IF EXISTS trg_messages_cascade_recipients")
        .expect("drop message cascade trigger for corruption fixture");
    conn.execute_sync(
        "DELETE FROM messages WHERE id = ?",
        &[Value::BigInt(orphaned_message_id)],
    )
    .expect("delete parent message while preserving recipient row");
    conn.execute_sync(
        "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) \
         VALUES (?, ?, 'to', NULL, NULL)",
        &[Value::BigInt(valid_message_id), Value::BigInt(990_001)],
    )
    .expect("insert missing-agent recipient row");
    conn.execute_sync(
        "INSERT OR IGNORE INTO inbox_stats \
         (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) \
         VALUES (?, 1, 1, 0, 1)",
        &[Value::BigInt(990_001)],
    )
    .expect("insert missing-agent inbox stats row");

    let report = check_schema_invariants_conn(&conn).expect("check invariants");
    assert_eq!(
        report.finding_count(SchemaInvariantKind::OrphanMessageRecipientMessage),
        1
    );
    assert_eq!(
        report.finding_count(SchemaInvariantKind::OrphanMessageRecipientAgent),
        1
    );
    assert_eq!(
        report.finding_count(SchemaInvariantKind::OrphanInboxStatsAgent),
        1
    );
}

#[test]
fn schema_invariants_detect_ack_before_read_and_bad_reservation_ttl() {
    let (pool, _dir) = make_pool();
    let project_id = setup_project(&pool, 200, 0);
    let sender_id = setup_agent(&pool, project_id, "RedFox");
    let recipient_id = setup_agent(&pool, project_id, "BlueLake");
    let message_id = send_msg(
        &pool,
        project_id,
        sender_id,
        recipient_id,
        "timestamp drift regression",
        Some("timestamp-drift"),
        true,
    );
    let reservation_id = create_reservation(
        &pool,
        project_id,
        sender_id,
        "src/schema_invariant_bad_ttl.rs",
    );

    let conn = fresh_conn(&pool);
    allow_corruption_fixture(&conn);
    conn.execute_sync(
        "UPDATE message_recipients SET read_ts = ?, ack_ts = ? \
         WHERE message_id = ? AND agent_id = ?",
        &[
            Value::BigInt(200),
            Value::BigInt(100),
            Value::BigInt(message_id),
            Value::BigInt(recipient_id),
        ],
    )
    .expect("force ack-before-read drift");
    conn.execute_sync(
        "UPDATE file_reservations SET expires_ts = created_ts - 1 WHERE id = ?",
        &[Value::BigInt(reservation_id)],
    )
    .expect("force reservation expiry drift");
    conn.execute_sync(
        "INSERT INTO file_reservation_releases (reservation_id, released_ts) \
         SELECT id, created_ts - 1 FROM file_reservations WHERE id = ?",
        &[Value::BigInt(reservation_id)],
    )
    .expect("force release-before-create drift");
    conn.execute_sync(
        "INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (?, ?)",
        &[Value::BigInt(990_002), Value::BigInt(1)],
    )
    .expect("insert orphan release row");

    let report = check_schema_invariants_conn(&conn).expect("check invariants");
    assert_eq!(report.finding_count(SchemaInvariantKind::AckBeforeRead), 1);
    assert_eq!(
        report.finding_count(SchemaInvariantKind::FileReservationExpiryBeforeCreate),
        1
    );
    assert_eq!(
        report.finding_count(SchemaInvariantKind::FileReservationReleaseBeforeCreate),
        1
    );
    assert_eq!(
        report.finding_count(SchemaInvariantKind::OrphanFileReservationRelease),
        1
    );
}

#[test]
fn schema_invariants_detect_contact_and_product_link_drift() {
    let (pool, _dir) = make_pool();
    let project_id = setup_project(&pool, 300, 0);
    let agent_id = setup_agent(&pool, project_id, "RedFox");
    let product_id = ensure_product_link(&pool, "schema-product-drift", project_id);

    let conn = fresh_conn(&pool);
    allow_corruption_fixture(&conn);
    conn.execute_sync(
        "INSERT INTO agent_links \
         (a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts, expires_ts) \
         VALUES (?, ?, ?, ?, 'pending', 'missing source', 10, 10, NULL)",
        &[
            Value::BigInt(project_id),
            Value::BigInt(990_003),
            Value::BigInt(project_id),
            Value::BigInt(agent_id),
        ],
    )
    .expect("insert missing-source contact link");
    conn.execute_sync(
        "INSERT INTO agent_links \
         (a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts, expires_ts) \
         VALUES (?, ?, ?, ?, 'mystery', 'missing target and invalid status', 20, 20, 19)",
        &[
            Value::BigInt(project_id),
            Value::BigInt(agent_id),
            Value::BigInt(project_id),
            Value::BigInt(990_004),
        ],
    )
    .expect("insert missing-target contact link");
    conn.execute_sync(
        "INSERT INTO product_project_links (product_id, project_id, created_at) VALUES (?, ?, ?)",
        &[
            Value::BigInt(990_005),
            Value::BigInt(project_id),
            Value::BigInt(1),
        ],
    )
    .expect("insert missing-product link");
    conn.execute_sync(
        "INSERT INTO product_project_links (product_id, project_id, created_at) VALUES (?, ?, ?)",
        &[
            Value::BigInt(product_id),
            Value::BigInt(990_006),
            Value::BigInt(2),
        ],
    )
    .expect("insert missing-project link");

    let report = check_schema_invariants_conn(&conn).expect("check invariants");
    assert_eq!(
        report.finding_count(SchemaInvariantKind::OrphanAgentLinkSource),
        1
    );
    assert_eq!(
        report.finding_count(SchemaInvariantKind::OrphanAgentLinkTarget),
        1
    );
    assert_eq!(
        report.finding_count(SchemaInvariantKind::InvalidAgentLinkStatus),
        1
    );
    assert_eq!(
        report.finding_count(SchemaInvariantKind::AgentLinkExpiryBeforeCreate),
        1
    );
    assert_eq!(
        report.finding_count(SchemaInvariantKind::OrphanProductProjectProduct),
        1
    );
    assert_eq!(
        report.finding_count(SchemaInvariantKind::OrphanProductProjectProject),
        1
    );
}

#[test]
fn deterministic_state_sequence_keeps_schema_invariants_healthy() {
    for seed in [
        0x05C0_FFEE_u64,
        0x000A_11CE_u64,
        0x0BAD_C0DE_u64,
        0x0D15_EA5E_u64,
    ] {
        run_stateful_sequence(seed, 24);
    }
}

// Heavier replay lane for local or nightly rch runs:
// rch exec -- cargo test -p mcp-agent-mail-db schema_invariant_heavy_replay_lane -- --ignored --nocapture
#[test]
#[ignore = "heavier deterministic replay lane; run the documented rch command"]
fn schema_invariant_heavy_replay_lane() {
    for seed in 0x51A7_E000_u64..0x51A7_E010_u64 {
        run_stateful_sequence(seed, 128);
    }
}
