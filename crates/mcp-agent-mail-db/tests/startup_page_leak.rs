//! GH#291: a freshly compacted (`VACUUM`ed, `freelist_count = 0`,
//! `integrity_check = ok`) mailbox must still be clean after one ordinary
//! service startup. The report observed a clean file re-acquiring a
//! contiguous run of `Page N: never used` orphans within ~235 ms of the first
//! open, before any client traffic. This test drives the exact startup path
//! (`create_pool` with migrations + the startup integrity check + a first
//! pooled write) against a scratch database and audits page accounting with
//! canonical SQLite at each stage so a regression names the stage that leaked.
//! Runtime phases self-execute this same test in fresh processes. The parent
//! audits only after each owning child exits, never against an idle live pool.

use crate::common;

use asupersync::Cx;
use mcp_agent_mail_db::sqlmodel_core::Error as SqlError;
use mcp_agent_mail_db::sqlmodel_sqlite::SqliteErrorCode;
use mcp_agent_mail_db::{CanonicalDbConn, DbPoolConfig, create_pool};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const TEST_NAME: &str =
    "startup_page_leak::clean_compacted_mailbox_stays_clean_across_service_startup";
const PHASE_ENV: &str = "AM_STARTUP_PAGE_LEAK_PHASE";
const ROOT_ENV: &str = "AM_STARTUP_PAGE_LEAK_ROOT";
const COOKIE_ENV: &str = "AM_STARTUP_PAGE_LEAK_COOKIE";

#[derive(Debug, Clone, PartialEq, Eq)]
struct PageAudit {
    page_count: i64,
    freelist_count: i64,
    unused_pages: Vec<i64>,
    other_findings: Vec<String>,
}

fn pragma_i64(conn: &CanonicalDbConn, sql: &str) -> Result<i64, Box<SqlError>> {
    let rows = conn.query_sync(sql, &[])?;
    let row = rows
        .first()
        .ok_or_else(|| SqlError::Custom(format!("{sql}: no rows")))?;
    Ok(row.get_as::<i64>(0)?)
}

/// Canonical-SQLite page audit of a quiesced database file.
fn audit(path: &Path) -> PageAudit {
    try_audit(path).unwrap_or_else(|error| panic!("canonical audit of {}: {error}", path.display())) // ubs:ignore — tests/it.rs-only assertion helper must fail a real canonical audit error
}

fn try_audit(path: &Path) -> Result<PageAudit, Box<SqlError>> {
    let conn = CanonicalDbConn::open_file(path.display().to_string())?;
    conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE)")?;
    let page_count = pragma_i64(&conn, "PRAGMA page_count")?;
    let freelist_count = pragma_i64(&conn, "PRAGMA freelist_count")?;
    let rows = conn.query_sync("PRAGMA integrity_check(1000000)", &[])?;
    let mut unused_pages = Vec::new();
    let mut other_findings = Vec::new();
    for row in &rows {
        let text = row.get_as::<String>(0)?;
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
    Ok(PageAudit {
        page_count,
        freelist_count,
        unused_pages,
        other_findings,
    })
}

fn sqlite_primary_error(error: &SqlError) -> Option<i32> {
    let source = match error {
        SqlError::Query(error) => error.source.as_deref(),
        SqlError::Connection(error) => error.source.as_deref(),
        _ => None,
    }?;
    source
        .downcast_ref::<SqliteErrorCode>()
        .map(|code| code.primary())
}

fn assert_corrupted_copy_is_rejected(root: &Path, source: &Path, clean: &PageAudit) {
    let conn = CanonicalDbConn::open_file(source.display().to_string()).expect("open clean source");
    let page_size = u64::try_from(pragma_i64(&conn, "PRAGMA page_size").unwrap()).unwrap();
    let rows = conn
        .query_sync(
            "SELECT rootpage FROM sqlite_master WHERE type='table' AND name='messages'",
            &[],
        )
        .expect("find the real messages btree root");
    let root_page = u64::try_from(rows[0].get_as::<i64>(0).unwrap()).unwrap();
    assert!(root_page > 0 && root_page <= u64::try_from(clean.page_count).unwrap());
    drop(conn);
    let source_image = std::fs::read(source).expect("retain the clean source bytes");

    // The source is checkpointed and has no owning runtime child. Copy its
    // physical image, retaining the header, then damage an actual btree page.
    let corrupted = root.join("corrupted-control.sqlite3");
    let mut target = mcp_agent_mail_core::disk::create_new_private_file_no_follow(&corrupted)
        .expect("create isolated corruption control");
    std::io::copy(&mut std::fs::File::open(source).unwrap(), &mut target).unwrap();
    let page_type_offset = (root_page - 1) * page_size + u64::from(root_page == 1) * 100;
    target.seek(SeekFrom::Start(page_type_offset)).unwrap();
    target.write_all(&[0xff]).unwrap();
    target.sync_all().unwrap();
    drop(target);

    match try_audit(&corrupted) {
        Ok(damaged) => assert!(
            !damaged.unused_pages.is_empty() || !damaged.other_findings.is_empty(),
            "the canonical oracle accepted a structurally corrupted image: {damaged:?}"
        ),
        Err(error) => assert_eq!(
            sqlite_primary_error(&error),
            Some(11),
            "only native SQLITE_CORRUPT qualifies; unrelated errors fail: {error}"
        ),
    }
    assert_eq!(
        &audit(source),
        clean,
        "the corruption control must leave its source audit untouched"
    );
    assert_eq!(
        std::fs::read(source).expect("read source after corruption control"),
        source_image,
        "the corruption control must leave its source bytes untouched"
    );
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

/// Every runtime owner lives in an actual child process, including seeding.
/// All three private variables are required so an incomplete invocation cannot
/// turn the ordinary test into a successful no-op.
fn run_child_phase_if_requested() -> bool {
    let phase = std::env::var_os(PHASE_ENV);
    let root = std::env::var_os(ROOT_ENV);
    let cookie = std::env::var_os(COOKIE_ENV);
    if phase.is_none() && root.is_none() && cookie.is_none() {
        return false;
    }
    let phase = phase
        .expect("child phase requires its private phase variable")
        .into_string()
        .expect("child phase is UTF-8");
    assert!(
        matches!(
            phase.as_str(),
            "seed" | "init" | "probe" | "write" | "restart"
        ),
        "unsupported child phase: {phase}"
    );
    let root = PathBuf::from(root.expect("child phase requires its private root variable"));
    let cookie = cookie
        .expect("child phase requires its private cookie variable")
        .into_string()
        .expect("child cookie is UTF-8");
    assert!(root.is_absolute(), "child root must be absolute");
    let root_meta = std::fs::symlink_metadata(&root).expect("child root metadata");
    assert!(root_meta.is_dir() && !root_meta.file_type().is_symlink());
    assert_eq!(std::fs::canonicalize(&root).unwrap(), root);
    let root_name = root.file_name().unwrap().to_str().unwrap();
    assert!(root_name.starts_with("am-startup-page-leak-"));
    assert_eq!(cookie, root_name, "child cookie must name its private root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(root_meta.permissions().mode() & 0o777, 0o700);
    }
    let storage_root = root.join("storage");
    let storage_meta = std::fs::symlink_metadata(&storage_root).expect("storage root metadata");
    assert!(storage_meta.is_dir() && !storage_meta.file_type().is_symlink());
    assert_eq!(std::fs::canonicalize(&storage_root).unwrap(), storage_root);
    let db_path = root.join("storage.sqlite3");
    if phase == "seed" {
        let missing =
            std::fs::symlink_metadata(&db_path).expect_err("seed must create its own database");
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
        seed_mailbox(&db_path, &storage_root);
    } else {
        let db_meta = std::fs::symlink_metadata(&db_path).expect("phase database metadata");
        assert!(db_meta.is_file() && !db_meta.file_type().is_symlink());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                db_meta.nlink(),
                1,
                "phase database must have one private name"
            );
        }
        let pool = create_pool(&pool_config(&db_path, &storage_root)).expect("create startup pool");
        let cx = Cx::for_testing();
        let conn = common::spin_poll(pool.acquire(&cx))
            .into_result()
            .expect("acquire startup connection");
        drop(conn);
        if matches!(phase.as_str(), "probe" | "write") {
            pool.run_startup_integrity_check()
                .expect("startup integrity check");
        }
        if phase == "write" {
            // Retain the continuous init -> integrity probe -> first write
            // path on this same pool, with no intervening canonical open.
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
        }
        drop(pool);
    }
    eprintln!("startup-page-leak-complete:{cookie}:{phase}");
    true
}

fn run_child_phase(phase: &str, root: &Path, cookie: &str) {
    let output =
        Command::new(std::env::current_exe().expect("current integration test executable"))
            .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
            .env(PHASE_ENV, phase)
            .env(ROOT_ENV, root)
            .env(COOKIE_ENV, cookie)
            .output()
            .expect("execute the original test in an owning child");
    // Replay both channels even when a child fails, before checking its status.
    std::io::stdout().write_all(&output.stdout).unwrap();
    std::io::stderr().write_all(&output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{phase} child failed: {}",
        output.status
    );
    let ack = format!("startup-page-leak-complete:{cookie}:{phase}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter(|line| *line == ack.as_str())
            .count(),
        1,
        "{phase} child must acknowledge its completed work; a zero-test filter is not success"
    );
}

#[test]
fn clean_compacted_mailbox_stays_clean_across_service_startup() {
    if run_child_phase_if_requested() {
        return;
    }
    let mut builder = tempfile::Builder::new();
    builder.prefix("am-startup-page-leak-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let dir = builder.tempdir().expect("private phase root");
    let root = std::fs::canonicalize(dir.path()).expect("canonical private phase root");
    let cookie = root.file_name().unwrap().to_str().unwrap();
    let storage_root = root.join("storage");
    std::fs::create_dir_all(&storage_root).expect("storage root");
    let db_path = root.join("storage.sqlite3");

    run_child_phase("seed", &root, cookie);
    let seeded = audit(&db_path);
    eprintln!("after seed: {seeded:?}");

    let clean = compact_and_verify(&db_path);
    eprintln!("after compaction: {clean:?}");
    assert_corrupted_copy_is_rejected(&root, &db_path, &clean);

    // Stage 1: pool creation + init gate (schema-version refusal, migrations,
    // legacy ATC table drop, runtime pragmas, FTS cleanup, schema gate,
    // startup data repairs) — everything `am serve-http` runs before it
    // reports the database open.
    run_child_phase("init", &root, cookie);
    let after_init = audit(&db_path);
    eprintln!("after init gate: {after_init:?}");
    assert_no_new_orphans("init gate", &clean, &after_init);

    // Stage 2: the startup integrity probe the server runs right after open.
    run_child_phase("probe", &root, cookie);
    let after_probe = audit(&db_path);
    eprintln!("after startup integrity check: {after_probe:?}");
    assert_no_new_orphans("startup integrity check", &clean, &after_probe);

    // Stage 3: first ordinary write through the pool after startup.
    run_child_phase("write", &root, cookie);
    let after_write = audit(&db_path);
    eprintln!("after first write: {after_write:?}");
    assert_no_new_orphans("first write after startup", &clean, &after_write);

    // Stage 4: a second cold start on the already-started file.
    run_child_phase("restart", &root, cookie);
    let after_restart = audit(&db_path);
    eprintln!("after second start: {after_restart:?}");
    assert_no_new_orphans("second start", &clean, &after_restart);
}
