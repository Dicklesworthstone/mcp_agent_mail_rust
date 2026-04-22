//! Criterion benchmark for the inbox-fetch hot path.
//!
//! Seeds an in-memory DB with 10K messages distributed across agents,
//! then measures `fetch_inbox_rows_from_conn` at various limits.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mcp_agent_mail_db::schema;
use mcp_agent_mail_db::sync::fetch_inbox_rows_from_conn;
use mcp_agent_mail_db::DbConn;
use sqlmodel_core::Value;

fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(asupersync::Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let cx = asupersync::Cx::for_testing();
    let rt = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    rt.block_on(f(cx))
}

fn seeded_conn(n_messages: usize) -> (DbConn, i64, Vec<i64>) {
    let conn = DbConn::open_memory().expect("open in-memory db");
    conn.execute_raw(schema::PRAGMA_DB_INIT_SQL)
        .expect("apply PRAGMAs");
    block_on({
        let conn = &conn;
        move |cx| async move {
            schema::migrate_to_latest_base(&cx, conn)
                .await
                .into_result()
                .expect("init schema migrations");
        }
    });

    conn.execute_sync(
        "INSERT INTO projects (slug, human_key, created_at) VALUES ('bench', '/tmp/bench', 1000000)",
        &[],
    )
    .expect("insert project");
    let project_id = last_id(&conn);

    let n_agents: usize = 20;
    let mut agent_ids = Vec::with_capacity(n_agents);
    for i in 0..n_agents {
        conn.execute_sync(
            "INSERT INTO agents (project_id, name, program, model, task_description, \
             inception_ts, last_active_ts) VALUES (?1, ?2, 'bench', 'bench', 'bench', 1000000, 1000000)",
            &[
                Value::BigInt(project_id),
                Value::Text(format!("Agent{i}")),
            ],
        )
        .expect("insert agent");
        agent_ids.push(last_id(&conn));
    }

    let base_ts: i64 = 1_700_000_000_000_000;
    for i in 0..n_messages {
        let sender_idx = i % n_agents;
        let ts = base_ts + i as i64 * 1_000_000;
        let importance = if i % 20 == 0 { "urgent" } else { "normal" };
        let ack_req: i64 = if i % 10 == 0 { 1 } else { 0 };
        conn.execute_sync(
            "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, \
             ack_required, thread_id, created_ts, recipients_json, attachments) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '[]', '[]')",
            &[
                Value::BigInt(project_id),
                Value::BigInt(agent_ids[sender_idx]),
                Value::Text(format!("Subject {i}")),
                Value::Text(format!("Body of message {i} with some realistic length content.")),
                Value::Text(importance.to_string()),
                Value::BigInt(ack_req),
                Value::Text(format!("thread-{}", i / 5)),
                Value::BigInt(ts),
            ],
        )
        .expect("insert message");
        let msg_id = last_id(&conn);

        let recipient_idx = (i + 1) % n_agents;
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'to')",
            &[
                Value::BigInt(msg_id),
                Value::BigInt(agent_ids[recipient_idx]),
            ],
        )
        .expect("insert recipient");

        if i % 3 == 0 {
            let cc_idx = (i + 2) % n_agents;
            conn.execute_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'cc')",
                &[
                    Value::BigInt(msg_id),
                    Value::BigInt(agent_ids[cc_idx]),
                ],
            )
            .expect("insert cc recipient");
        }
    }

    (conn, project_id, agent_ids)
}

fn last_id(conn: &DbConn) -> i64 {
    conn.query_sync("SELECT last_insert_rowid() AS id", &[])
        .expect("query last id")
        .into_iter()
        .next()
        .and_then(|r| r.get_named::<i64>("id").ok())
        .expect("get last id")
}

fn bench_inbox_fetch(c: &mut Criterion) {
    let (conn, project_id, agent_ids) = seeded_conn(10_000);
    let target_agent = agent_ids[1];

    let mut group = c.benchmark_group("inbox_fetch_10k");
    group.sample_size(100);

    for limit in [50, 200, 1000] {
        group.bench_with_input(
            BenchmarkId::new("limit", limit),
            &limit,
            |b, &lim| {
                b.iter(|| {
                    black_box(
                        fetch_inbox_rows_from_conn(
                            &conn,
                            project_id,
                            target_agent,
                            false,
                            false,
                            false,
                            None,
                            lim,
                        )
                        .expect("inbox fetch"),
                    )
                });
            },
        );
    }

    group.bench_function("urgent_only", |b| {
        b.iter(|| {
            black_box(
                fetch_inbox_rows_from_conn(
                    &conn,
                    project_id,
                    target_agent,
                    true,
                    false,
                    false,
                    None,
                    200,
                )
                .expect("inbox fetch urgent"),
            )
        });
    });

    group.bench_function("unread_only", |b| {
        b.iter(|| {
            black_box(
                fetch_inbox_rows_from_conn(
                    &conn,
                    project_id,
                    target_agent,
                    false,
                    true,
                    false,
                    None,
                    200,
                )
                .expect("inbox fetch unread"),
            )
        });
    });

    group.finish();
}

fn bench_inbox_fetch_since(c: &mut Criterion) {
    let (conn, project_id, agent_ids) = seeded_conn(10_000);
    let target_agent = agent_ids[1];
    let midpoint_ts: i64 = 1_700_000_000_000_000 + 5000 * 1_000_000;

    c.bench_function("inbox_fetch_10k_since_midpoint", |b| {
        b.iter(|| {
            black_box(
                fetch_inbox_rows_from_conn(
                    &conn,
                    project_id,
                    target_agent,
                    false,
                    false,
                    false,
                    Some(midpoint_ts),
                    200,
                )
                .expect("inbox fetch since"),
            )
        });
    });
}

fn bench_inbox_stats_query(c: &mut Criterion) {
    let (conn, project_id, agent_ids) = seeded_conn(10_000);
    let target_agent = agent_ids[1];

    c.bench_function("inbox_count_query_10k", |b| {
        b.iter(|| {
            let rows = conn
                .query_sync(
                    "SELECT COUNT(*) AS cnt FROM message_recipients r \
                     JOIN messages m ON m.id = r.message_id \
                     WHERE r.agent_id = ? AND m.project_id = ?",
                    &[Value::BigInt(target_agent), Value::BigInt(project_id)],
                )
                .expect("count query");
            black_box(rows);
        });
    });
}

criterion_group!(
    benches,
    bench_inbox_fetch,
    bench_inbox_fetch_since,
    bench_inbox_stats_query,
);
criterion_main!(benches);
