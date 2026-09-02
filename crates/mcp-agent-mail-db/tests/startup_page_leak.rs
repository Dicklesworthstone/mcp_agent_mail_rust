//! GH#291: a freshly compacted (`VACUUM`ed, `freelist_count = 0`,
//! `integrity_check = ok`) mailbox must still be clean after one ordinary
//! service startup. The report observed a clean file re-acquiring a
//! contiguous run of `Page N: never used` orphans within ~235 ms of the first
//! open, before any client traffic. This test drives the exact startup path
//! (`create_pool` with migrations + the startup integrity check + a first
//! pooled write) against a scratch database and audits page accounting with
//! canonical SQLite at each stage so a regression names the stage that leaked.

mod common;

use asupersync::Cx;
use mcp_agent_mail_db::{CanonicalDbConn, DbPoolConfig, create_pool};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PageAudit {
    page_count: i64,
    freelist_count: i64,
    unused_pages: Vec<i64>,
    other_findings: Vec<String>,
}

fn pragma_i64(conn: &CanonicalDbConn, sql: &str) -> i64 {
    let rows = conn
        .query_sync(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let row = rows.first().unwrap_or_else(|| panic!("{sql}: no rows"));
    row.get_as::<i64>(0)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Canonical-SQLite page audit of a quiesced database file.
fn audit(path: &Path) -> PageAudit {
    let conn = CanonicalDbConn::open_file(path.display().to_string()).expect("open canonical");
    conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint");
    let page_count = pragma_i64(&conn, "PRAGMA page_count");
    let freelist_count = pragma_i64(&conn, "PRAGMA freelist_count");
    let rows = conn
        .query_sync("PRAGMA integrity_check(1000000)", &[])
        .expect("integrity_check");
    let mut unused_pages = Vec::new();
    let mut other_findings = Vec::new();
    for row in &rows {
        let text = row.get_as::<String>(0).unwrap_or_default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line == "ok" || line.starts_with("***") {
                continue;
            }
            if let Some(rest) = line.strip_prefix("Page ")
                && let Some(page) = rest
                    .strip_suffix(": never used")
                    .and_then(|n| n.parse::<i64>().ok())
            {
                unused_pages.push(page);
                continue;
            }
            other_findings.push(line.to_string());
        }
    }
    PageAudit {
        page_count,
        freelist_count,
        unused_pages,
        other_findings,
    }
}

fn pool_config(db_path: &Path, storage_root: &Path) -> DbPoolConfig {
    DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        storage_root: Some(storage_root.to_path_buf()),
        min_connections: 1,
        max_connections: 2,
        run_migrations: true,
        warmup_connections: 0,
        ..Default::default()
    }
}

/// Seed a realistic mailbox shape through the runtime engine: one project,
/// a few agents, and enough messages/recipients that the tables and their
/// indexes span many pages (so any startup rewrite has something to leak).
fn seed_mailbox(db_path: &Path, storage_root: &Path) {
    let pool = create_pool(&pool_config(db_path, storage_root)).expect("create seed pool");
    let cx = Cx::for_testing();
    let conn = common::spin_poll(pool.acquire(&cx))
        .into_result()
        .expect("acquire seed connection");
    conn.execute_raw(
        "INSERT INTO projects (slug, human_key, created_at) VALUES ('gh291', '/tmp/gh291', 1)",
    )
    .expect("insert project");
    for agent in 1..=8 {
        conn.execute_raw(&format!(
            "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) \
             VALUES (1, 'Agent{agent}', 'codex', 'gpt', 1, 1)"
        ))
        .expect("insert agent");
    }
    let body = "x".repeat(600);
    for message in 1..=400_i64 {
        let sender = (message % 8) + 1;
        conn.execute_raw(&format!(
            "INSERT INTO messages (project_id, sender_id, thread_id, subject, body_md, created_ts) \
             VALUES (1, {sender}, 'thread-{}', 'subject {message}', '{body}', {})",
            message % 20,
            1_700_000_000_000_000_i64 + message
        ))
        .expect("insert message");
        for recipient in 1..=3_i64 {
            let agent = ((message + recipient) % 8) + 1;
            if agent == sender {
                continue;
            }
            conn.execute_raw(&format!(
                "INSERT OR IGNORE INTO message_recipients (message_id, agent_id, kind) \
                 VALUES ({message}, {agent}, 'to')"
            ))
            .expect("insert recipient");
        }
    }
    drop(conn);
    drop(pool);
}

/// Compact the quiesced file exactly the way the report did: VACUUM, back to
/// WAL, and prove the result clean with canonical SQLite.
fn compact_and_verify(db_path: &Path) -> PageAudit {
    let conn = CanonicalDbConn::open_file(db_path.display().to_string()).expect("open canonical");
    conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint before vacuum");
    conn.execute_raw("VACUUM").expect("vacuum");
    let mode = conn
        .query_sync("PRAGMA journal_mode = WAL", &[])
        .expect("journal_mode wal");
    let mode = mode
        .first()
        .and_then(|row| row.get_as::<String>(0).ok())
        .unwrap_or_default();
    assert_eq!(mode.to_ascii_lowercase(), "wal");
    drop(conn);
    let clean = audit(db_path);
    assert!(
        clean.unused_pages.is_empty() && clean.other_findings.is_empty(),
        "compacted file must be clean before the startup under test: {clean:?}"
    );
    assert_eq!(
        clean.freelist_count, 0,
        "VACUUM must leave an empty freelist"
    );
    clean
}

fn assert_no_new_orphans(stage: &str, baseline: &PageAudit, after: &PageAudit) {
    assert!(
        after.other_findings.is_empty(),
        "{stage}: structural integrity findings after startup: {:?}",
        after.other_findings
    );
    assert!(
        after.unused_pages.is_empty(),
        "{stage}: startup orphaned {} page(s) (range {:?}..={:?}) on a clean file; \
         page_count {} -> {}, freelist_count {} -> {}",
        after.unused_pages.len(),
        after.unused_pages.first(),
        after.unused_pages.last(),
        baseline.page_count,
        after.page_count,
        baseline.freelist_count,
        after.freelist_count
    );
}

#[test]
fn clean_compacted_mailbox_stays_clean_across_service_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage_root = dir.path().join("storage");
    std::fs::create_dir_all(&storage_root).expect("storage root");
    let db_path = dir.path().join("storage.sqlite3");

    seed_mailbox(&db_path, &storage_root);
    let seeded = audit(&db_path);
    eprintln!("after seed: {seeded:?}");

    let clean = compact_and_verify(&db_path);
    eprintln!("after compaction: {clean:?}");

    // Stage 1: pool creation + init gate (schema-version refusal, migrations,
    // legacy ATC table drop, runtime pragmas, FTS cleanup, schema gate,
    // startup data repairs) — everything `am serve-http` runs before it
    // reports the database open.
    let pool = create_pool(&pool_config(&db_path, &storage_root)).expect("create startup pool");
    let cx = Cx::for_testing();
    let conn = common::spin_poll(pool.acquire(&cx))
        .into_result()
        .expect("acquire startup connection");
    drop(conn);
    let after_init = audit(&db_path);
    eprintln!("after init gate: {after_init:?}");
    assert_no_new_orphans("init gate", &clean, &after_init);

    // Stage 2: the startup integrity probe the server runs right after open.
    pool.run_startup_integrity_check()
        .expect("startup integrity check");
    let after_probe = audit(&db_path);
    eprintln!("after startup integrity check: {after_probe:?}");
    assert_no_new_orphans("startup integrity check", &clean, &after_probe);

    // Stage 3: first ordinary write through the pool after startup.
    let conn = common::spin_poll(pool.acquire(&cx))
        .into_result()
        .expect("acquire write connection");
    conn.execute_raw(
        "INSERT INTO messages (project_id, sender_id, subject, body_md, created_ts) \
         VALUES (1, 1, 'post-startup', 'hello', 1700000000000401)",
    )
    .expect("post-startup insert");
    conn.execute_raw(
        "INSERT INTO message_recipients (message_id, agent_id, kind) \
         SELECT MAX(id), 2, 'to' FROM messages",
    )
    .expect("post-startup recipient");
    drop(conn);
    drop(pool);
    let after_write = audit(&db_path);
    eprintln!("after first write: {after_write:?}");
    assert_no_new_orphans("first write after startup", &clean, &after_write);

    // Stage 4: a second cold start on the already-started file.
    let pool = create_pool(&pool_config(&db_path, &storage_root)).expect("create second pool");
    let conn = common::spin_poll(pool.acquire(&cx))
        .into_result()
        .expect("acquire second-start connection");
    drop(conn);
    drop(pool);
    let after_restart = audit(&db_path);
    eprintln!("after second start: {after_restart:?}");
    assert_no_new_orphans("second start", &clean, &after_restart);
}
