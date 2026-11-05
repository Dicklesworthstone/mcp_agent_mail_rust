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
use mcp_agent_mail_db::{CanonicalDbConn, DbConn, DbPoolConfig, create_pool};
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

#[derive(Debug, Default)]
struct MetricsIndexShape {
    pages: usize,
    leaves: usize,
    single_cell_leaves: usize,
    depth: usize,
}

/// Inspect only the affected index, without depending on SQLite's optional
/// dbstat extension. Interior index cells start with a four-byte child page
/// number; leaf headers carry their cell count. A visited set makes cycles a
/// test failure rather than an unbounded walk.
fn metrics_index_shape(path: &Path, root_page: usize) -> MetricsIndexShape {
    let bytes = std::fs::read(path).expect("read checkpointed metrics database");
    assert!(bytes.starts_with(b"SQLite format 3\0"));
    let page_size = match u16::from_be_bytes([bytes[16], bytes[17]]) {
        1 => 65_536,
        size => usize::from(size),
    };
    let mut pending = vec![(root_page, 1)];
    let mut visited = std::collections::HashSet::new();
    let mut shape = MetricsIndexShape::default();
    while let Some((page_number, depth)) = pending.pop() {
        assert!(
            visited.insert(page_number),
            "index page cycle at {page_number}"
        );
        assert!(page_number > 0 && page_number <= bytes.len() / page_size);
        let page = &bytes[(page_number - 1) * page_size..page_number * page_size];
        let header = usize::from(page_number == 1) * 100;
        let cells = usize::from(u16::from_be_bytes([page[header + 3], page[header + 4]]));
        shape.pages += 1;
        shape.depth = shape.depth.max(depth);
        match page[header] {
            0x0a => {
                shape.leaves += 1;
                shape.single_cell_leaves += usize::from(cells == 1);
            }
            0x02 => {
                let right_child =
                    u32::from_be_bytes(page[header + 8..header + 12].try_into().unwrap());
                pending.push((usize::try_from(right_child).unwrap(), depth + 1));
                for cell in 0..cells {
                    let pointer = header + 12 + cell * 2;
                    let offset =
                        usize::from(u16::from_be_bytes([page[pointer], page[pointer + 1]]));
                    let child = u32::from_be_bytes(page[offset..offset + 4].try_into().unwrap());
                    pending.push((usize::try_from(child).unwrap(), depth + 1));
                }
            }
            other => panic!("index contains non-index page {page_number}: {other:#x}"),
        }
    }
    shape
}

fn seed_legacy_metrics_fixture(path: &Path, page_size: i64, tool: &str) {
    use mcp_agent_mail_db::sqlmodel::Value;

    // Canonical SQLite supplies an existing file with the unchanged v9
    // schema and one old sample. The upgraded runtime must append without
    // rebuilding the schema or losing that pre-existing row.
    let seed = CanonicalDbConn::open_file(path.to_str().unwrap()).unwrap();
    seed.execute_raw(&format!("PRAGMA page_size = {page_size}"))
        .unwrap();
    let migrations: Vec<_> = mcp_agent_mail_db::schema::schema_migrations()
        .into_iter()
        .filter(|migration| migration.id.starts_with("v9_"))
        .collect();
    assert_eq!(migrations.len(), 3, "exercise all three metrics migrations");
    for migration in migrations {
        seed.execute_raw(&migration.up).unwrap();
    }
    seed.execute_sync(
        "INSERT INTO tool_metrics_snapshots (tool_name, collected_ts, calls) VALUES (?, 0, 7)",
        &[Value::Text(tool.to_string())],
    )
    .unwrap();
}

/// GH#372 / `FrankenSQLite` #521: run the persisted metrics migration's actual
/// composite DESC index through repeated leaf and interior splits. The old
/// engine passed `integrity_check` while allocating almost one leaf per two rows.
#[test]
fn descending_metrics_insertions_keep_index_pages_dense() {
    use mcp_agent_mail_db::sqlmodel::Value;

    const ROWS: i64 = 12_000;
    let dir = tempfile::tempdir().expect("descending metrics fixture directory");
    for page_size in [512, 4096] {
        let path = dir.path().join(format!("metrics-{page_size}.sqlite3"));
        let tool = format!("send_message_{}", "x".repeat(48));
        seed_legacy_metrics_fixture(&path, page_size, &tool);

        let writer = DbConn::open_file(path.to_str().unwrap()).expect("open existing metrics file");
        let actual_page_size = writer.query_sync("PRAGMA page_size", &[]).unwrap()[0]
            .get_as::<i64>(0)
            .unwrap();
        assert_eq!(actual_page_size, page_size);
        writer.execute_raw("BEGIN").unwrap();
        for timestamp in 1..=ROWS {
            writer
                .execute_sync(
                    "INSERT INTO tool_metrics_snapshots (tool_name, collected_ts, calls) \
                     VALUES (?, ?, ?)",
                    &[
                        Value::Text(tool.clone()),
                        Value::BigInt(timestamp),
                        Value::BigInt(timestamp * 3),
                    ],
                )
                .unwrap_or_else(|error| panic!("DESC insertion {timestamp}: {error}"));
            if timestamp % 100 == 0 {
                writer.execute_raw("COMMIT").unwrap();
                if timestamp != ROWS {
                    writer.execute_raw("BEGIN").unwrap();
                }
            }
        }
        writer
            .close_sync()
            .expect("close and flush descending metrics fixture");

        let canonical = CanonicalDbConn::open_file(path.to_str().unwrap()).unwrap();
        canonical
            .execute_raw("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let integrity = canonical.query_sync("PRAGMA integrity_check", &[]).unwrap();
        assert_eq!(integrity.len(), 1);
        assert_eq!(integrity[0].get_as::<String>(0).unwrap(), "ok");
        let ordered = canonical
            .query_sync(
                "SELECT collected_ts, calls FROM tool_metrics_snapshots \
                 INDEXED BY idx_tool_metrics_snapshots_tool_ts \
                 WHERE tool_name = ? ORDER BY collected_ts DESC",
                &[Value::Text(tool)],
            )
            .expect("canonical reader must traverse the persisted DESC index");
        assert_eq!(ordered.len(), usize::try_from(ROWS + 1).unwrap());
        for (position, row) in ordered.iter().enumerate() {
            let timestamp = ROWS - i64::try_from(position).unwrap();
            assert_eq!(row.get_as::<i64>(0).unwrap(), timestamp);
            assert_eq!(
                row.get_as::<i64>(1).unwrap(),
                if timestamp == 0 { 7 } else { timestamp * 3 }
            );
        }
        let root = canonical
            .query_sync(
                "SELECT rootpage FROM sqlite_master \
                 WHERE name = 'idx_tool_metrics_snapshots_tool_ts'",
                &[],
            )
            .unwrap()[0]
            .get_as::<i64>(0)
            .unwrap();
        drop(canonical);

        let shape = metrics_index_shape(&path, usize::try_from(root).unwrap());
        let file_bytes = std::fs::metadata(&path).unwrap().len();
        eprintln!("GH372 page_size={page_size} rows={ROWS} bytes={file_bytes} index={shape:?}");
        assert!(
            shape.depth >= 3,
            "fixture must split interior index pages: {shape:?}"
        );
        assert!(shape.leaves > 10, "fixture must exercise many leaf splits");
        assert!(
            shape.single_cell_leaves * 10 < shape.leaves,
            "DESC insertions left mostly one-cell leaves despite integrity_check=ok: {shape:?}"
        );
        assert!(
            file_bytes <= u64::try_from(ROWS).unwrap() * 384 + 128 * 1024,
            "metrics database amplified storage beyond 384 bytes/sample: {file_bytes}"
        );
    }
}

#[cfg(target_os = "linux")]
const MEMORY_CHILD_ENV: &str = "AM_GH372_MEMORY_FIXTURE";
#[cfg(target_os = "linux")]
const MEMORY_CHILD_TEST: &str =
    "startup_page_leak::existing_mailbox_reads_have_bounded_connection_memory";
#[cfg(target_os = "linux")]
const MEMORY_CHILD_ACK: &str = "gh372-memory-probe-complete";

#[cfg(target_os = "linux")]
fn process_memory_kib(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .expect("Linux memory accounting is required for the memory regression")
        .lines()
        .find_map(|line| {
            line.strip_prefix(field)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or_else(|| panic!("missing {field} in /proc/self/status"))
}

#[cfg(target_os = "linux")]
fn log_connection_memory(stage: &str, connection: usize) {
    let rss_kib = process_memory_kib("VmRSS:");
    let peak_kib = process_memory_kib("VmHWM:");
    let rss_anon_kib = process_memory_kib("RssAnon:");
    let rss_file_kib = process_memory_kib("RssFile:");
    let rss_shmem_kib = process_memory_kib("RssShmem:");
    eprintln!(
        "GH372 stage={stage} connection={connection} rss_kib={rss_kib} peak_kib={peak_kib} \
         rss_anon_kib={rss_anon_kib} rss_file_kib={rss_file_kib} rss_shmem_kib={rss_shmem_kib}"
    );
}

fn seed_legacy_memory_mailbox(path: &Path, message_count: i64) -> String {
    seed_legacy_memory_mailbox_with_body_scale(path, message_count, 1)
}

fn seed_legacy_memory_mailbox_with_body_scale(
    path: &Path,
    message_count: i64,
    unread_body_scale: usize,
) -> String {
    use mcp_agent_mail_db::sqlmodel::Value;

    assert!(unread_body_scale > 0);
    let seed = CanonicalDbConn::open_file(path.to_str().unwrap()).unwrap();
    seed.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
        .expect("seed the unchanged mailbox schema using canonical SQLite");
    seed.execute_raw(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'gh372', '/gh372', 1); \
         INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) \
         VALUES (1, 1, 'BlueLake', 'codex', 'test', 1, 1); BEGIN",
    )
    .unwrap();
    let body = "retained body ".repeat(320);
    let unread_body = body.repeat(unread_body_scale);
    for id in 1..=message_count {
        seed.execute_sync(
            "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts) \
             VALUES (?, 1, 1, 'before upgrade', ?, ?)",
            &[
                Value::BigInt(id),
                Value::Text(if id == 1 {
                    body.clone()
                } else {
                    unread_body.clone()
                }),
                Value::BigInt(id),
            ],
        )
        .unwrap();
    }
    seed.execute_raw("COMMIT").unwrap();
    body
}

#[cfg(target_os = "linux")]
fn run_bounded_connection_memory_child(path: &Path) -> u64 {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            MEMORY_CHILD_TEST,
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(MEMORY_CHILD_ENV, path)
        .output()
        .expect("run isolated memory measurement");
    std::io::stdout().write_all(&output.stdout).unwrap();
    std::io::stderr().write_all(&output.stderr).unwrap();
    assert!(
        output.status.success(),
        "memory child failed: {}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr
            .lines()
            .filter(|line| *line == MEMORY_CHILD_ACK)
            .count(),
        1,
        "a zero-test child filter is not a successful memory regression"
    );
    let mut summaries = stderr
        .lines()
        .filter(|line| line.starts_with("GH372 baseline_kib="));
    let summary = summaries.next().expect("memory child must report its peak");
    assert!(
        summaries.next().is_none(),
        "expected exactly one memory measurement"
    );
    summary
        .split_whitespace()
        .find_map(|field| field.strip_prefix("delta_kib="))
        .expect("memory measurement must include its pre-open baseline delta")
        .parse()
        .expect("memory delta must be a number of KiB")
}

/// GH#372 / br-kp1in.46: 0.4.10 multiplied each mailbox's logical image by
/// its connection count, even for tiny reads. Use a fresh process so an earlier
/// test's allocator high-water mark cannot hide another whole-image allocation.
#[cfg(target_os = "linux")]
#[test]
fn existing_mailbox_reads_have_bounded_connection_memory() {
    use mcp_agent_mail_db::sqlmodel::Value;

    const MESSAGE_COUNT: i64 = 4096;
    if let Some(path) = std::env::var_os(MEMORY_CHILD_ENV) {
        let path = PathBuf::from(path);
        assert!(path.is_absolute());
        assert_eq!(std::fs::canonicalize(&path).unwrap(), path);
        assert!(
            path.parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("am-gh372-memory-"))
        );
        let baseline_kib = process_memory_kib("VmRSS:");
        log_connection_memory("baseline", 0);
        let writer = DbConn::open_file(path.to_str().unwrap()).expect("open existing mailbox");
        log_connection_memory("writer_open", 0);
        let readers: Vec<_> = (0..4)
            .map(|index| {
                let reader =
                    DbConn::open_file(path.to_str().unwrap()).expect("open runtime reader");
                log_connection_memory("reader_open", index + 1);
                reader
            })
            .collect();
        writer
            .execute_raw("UPDATE messages SET subject = 'post-upgrade write' WHERE id = 1")
            .expect("upgraded writer must preserve the existing mailbox format");
        log_connection_memory("writer_commit", 0);
        for (index, reader) in readers.iter().enumerate() {
            assert_eq!(
                reader.query_sync("SELECT 1", &[]).unwrap()[0]
                    .get_as::<i64>(0)
                    .unwrap(),
                1
            );
            log_connection_memory("constant_read", index + 1);
            let row = reader
                .query_sync(
                    "SELECT subject, body_md FROM messages WHERE id = ?",
                    &[Value::BigInt(1)],
                )
                .expect("read committed message through another connection");
            assert_eq!(row.len(), 1, "point lookup must return exactly one message");
            assert_eq!(row[0].get_as::<String>(0).unwrap(), "post-upgrade write");
            assert_eq!(
                row[0].get_as::<String>(1).unwrap(),
                "retained body ".repeat(320)
            );
            drop(row);
            log_connection_memory("point_read_complete", index + 1);
        }
        let peak_kib = process_memory_kib("VmHWM:");
        let delta_kib = peak_kib.saturating_sub(baseline_kib);
        eprintln!("GH372 baseline_kib={baseline_kib} peak_kib={peak_kib} delta_kib={delta_kib}");
        // The file holds >16 MiB of message bodies. Five live connections may
        // use bounded pager caches, but must not each hydrate the entire image.
        assert!(
            delta_kib <= 32 * 1024,
            "small reads amplified mailbox RSS by {delta_kib} KiB"
        );
        drop(readers);
        log_connection_memory("readers_dropped", 0);
        writer
            .close_sync()
            .expect("close and flush bounded-memory fixture");
        log_connection_memory("writer_closed", 0);
        eprintln!("{MEMORY_CHILD_ACK}");
        return;
    }

    let dir = tempfile::Builder::new()
        .prefix("am-gh372-memory-")
        .tempdir()
        .expect("private memory fixture directory");
    let path = std::fs::canonicalize(dir.path())
        .unwrap()
        .join("storage.sqlite3");
    let body = seed_legacy_memory_mailbox(&path, MESSAGE_COUNT);
    assert!(std::fs::metadata(&path).unwrap().len() > 16 * 1024 * 1024);
    run_bounded_connection_memory_child(&path);
    let canonical = CanonicalDbConn::open_file(path.to_str().unwrap()).unwrap();
    let rows = canonical
        .query_sync("SELECT COUNT(*), SUM(length(body_md)) FROM messages", &[])
        .unwrap();
    assert_eq!(rows[0].get_as::<i64>(0).unwrap(), MESSAGE_COUNT);
    assert_eq!(
        rows[0].get_as::<i64>(1).unwrap(),
        MESSAGE_COUNT * i64::try_from(body.len()).unwrap()
    );
}

/// GH#372: grow only unread message bodies so index-entry counts, index keys,
/// connection counts and point-read results stay fixed. Both isolated children
/// must meet the original 32 MiB bound, including first-open integrity work.
#[cfg(target_os = "linux")]
#[test]
fn existing_mailbox_reads_remain_bounded_with_larger_unread_bodies() {
    use mcp_agent_mail_db::sqlmodel::Value;

    const MESSAGE_COUNT: i64 = 4096;
    let dir = tempfile::Builder::new()
        .prefix("am-gh372-memory-")
        .tempdir()
        .expect("private archive-size fixture directory");
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut deltas_kib = Vec::new();
    for unread_body_scale in [1, 4] {
        let path = root.join(format!("body-scale-{unread_body_scale}.sqlite3"));
        let body =
            seed_legacy_memory_mailbox_with_body_scale(&path, MESSAGE_COUNT, unread_body_scale);
        let body_bytes = i64::try_from(body.len()).unwrap();
        let expected_body_bytes =
            body_bytes * (1 + (MESSAGE_COUNT - 1) * i64::try_from(unread_body_scale).unwrap());
        assert!(std::fs::metadata(&path).unwrap().len() > 16 * 1024 * 1024);
        if unread_body_scale == 4 {
            assert!(expected_body_bytes > 64 * 1024 * 1024);
        }
        deltas_kib.push(run_bounded_connection_memory_child(&path));

        let canonical = CanonicalDbConn::open_file(path.to_str().unwrap())
            .expect("reopen measured mailbox with canonical SQLite");
        let totals = canonical
            .query_sync("SELECT COUNT(*), SUM(length(body_md)) FROM messages", &[])
            .expect("verify measured mailbox row and payload counts");
        assert_eq!(totals[0].get_as::<i64>(0).unwrap(), MESSAGE_COUNT);
        assert_eq!(totals[0].get_as::<i64>(1).unwrap(), expected_body_bytes);
        let matching_bodies = canonical
            .query_sync(
                "SELECT COUNT(*) FROM messages \
                 WHERE body_md = CASE WHEN id = 1 THEN ? ELSE ? END",
                &[
                    Value::Text(body.clone()),
                    Value::Text(body.repeat(unread_body_scale)),
                ],
            )
            .expect("verify every unread body survived native opens and the point update");
        assert_eq!(matching_bodies[0].get_as::<i64>(0).unwrap(), MESSAGE_COUNT);
        let integrity = canonical
            .query_sync("PRAGMA integrity_check", &[])
            .expect("verify measured mailbox integrity with canonical SQLite");
        assert_eq!(integrity.len(), 1);
        assert_eq!(integrity[0].get_as::<String>(0).unwrap(), "ok");
    }
    eprintln!(
        "GH372 unread_body_scale=4 small_delta_kib={} large_delta_kib={}",
        deltas_kib[0], deltas_kib[1]
    );
}

/// GH#372: reducing connection memory must preserve an active reader's snapshot,
/// refresh it after the transaction ends, and keep aborted writes invisible.
#[test]
fn existing_mailbox_readers_preserve_snapshots_and_durable_writes() {
    use mcp_agent_mail_db::sqlmodel::Value;

    let dir = tempfile::tempdir().expect("snapshot fixture directory");
    let path = dir.path().join("snapshot-mailbox.sqlite3");
    let path_str = path.to_str().unwrap();
    let original = (
        "before upgrade".to_owned(),
        seed_legacy_memory_mailbox(&path, 1),
    );

    let writer = DbConn::open_file(path_str).expect("open native writer");
    let journal = writer
        .query_sync("PRAGMA journal_mode = WAL", &[])
        .expect("enable concurrent WAL readers");
    assert_eq!(
        journal[0].get_as::<String>(0).unwrap().to_ascii_lowercase(),
        "wal"
    );
    let snapshot_reader = DbConn::open_file(path_str).expect("open snapshot reader");
    let current_reader = DbConn::open_file(path_str).expect("open autocommit reader");
    let read_message = |conn: &DbConn| {
        let rows = conn
            .query_sync(
                "SELECT subject, body_md FROM messages WHERE id = ?",
                &[Value::BigInt(1)],
            )
            .expect("read message through native connection");
        assert_eq!(rows.len(), 1);
        (
            rows[0].get_as::<String>(0).unwrap(),
            rows[0].get_as::<String>(1).unwrap(),
        )
    };
    let committed = ("committed subject".to_owned(), "committed body".to_owned());
    let aborted = ("aborted subject".to_owned(), "aborted body".to_owned());

    snapshot_reader
        .execute_raw("BEGIN DEFERRED")
        .expect("begin reader snapshot");
    assert_eq!(read_message(&snapshot_reader), original);
    assert_eq!(read_message(&current_reader), original);
    writer
        .execute_raw("BEGIN IMMEDIATE")
        .expect("begin concurrent writer");
    writer
        .execute_sync(
            "UPDATE messages SET subject = ?, body_md = ? WHERE id = 1",
            &[
                Value::Text(committed.0.clone()),
                Value::Text(committed.1.clone()),
            ],
        )
        .expect("update message while reader holds snapshot");
    writer
        .execute_raw("COMMIT")
        .expect("commit updated message");

    // Both readers predate the write. Only the explicit transaction retains
    // the old view; sharing a mutable image between them would violate this.
    assert_eq!(read_message(&snapshot_reader), original);
    assert_eq!(read_message(&current_reader), committed);
    snapshot_reader
        .execute_raw("COMMIT")
        .expect("end reader snapshot");
    assert_eq!(read_message(&snapshot_reader), committed);

    writer
        .execute_raw("BEGIN IMMEDIATE")
        .expect("begin write that will be rolled back");
    writer
        .execute_sync(
            "UPDATE messages SET subject = ?, body_md = ? WHERE id = 1",
            &[
                Value::Text(aborted.0.clone()),
                Value::Text(aborted.1.clone()),
            ],
        )
        .expect("stage uncommitted message");
    assert_eq!(read_message(&writer), aborted);
    assert_eq!(read_message(&snapshot_reader), committed);
    assert_eq!(read_message(&current_reader), committed);
    writer
        .execute_raw("ROLLBACK")
        .expect("roll back staged write");
    assert_eq!(read_message(&writer), committed);
    assert_eq!(read_message(&snapshot_reader), committed);
    assert_eq!(read_message(&current_reader), committed);
    snapshot_reader.close_sync().expect("close snapshot reader");
    current_reader
        .close_sync()
        .expect("close autocommit reader");
    writer.close_sync().expect("flush and close writer");

    // A different engine verifies the persisted values only after every native
    // connection has closed, so an in-process cache cannot satisfy the oracle.
    let canonical = CanonicalDbConn::open_file(path_str).expect("reopen with canonical SQLite");
    let rows = canonical
        .query_sync("SELECT subject, body_md FROM messages ORDER BY id", &[])
        .expect("read durable message with canonical SQLite");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get_as::<String>(0).unwrap(), committed.0);
    assert_eq!(rows[0].get_as::<String>(1).unwrap(), committed.1);
    let integrity = canonical
        .query_sync("PRAGMA integrity_check", &[])
        .expect("check canonical mailbox integrity");
    assert_eq!(integrity.len(), 1);
    assert_eq!(integrity[0].get_as::<String>(0).unwrap(), "ok");
    assert!(
        canonical
            .query_sync("PRAGMA foreign_key_check", &[])
            .expect("check canonical mailbox foreign keys")
            .is_empty()
    );
}
