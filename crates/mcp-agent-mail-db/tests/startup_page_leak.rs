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
fn memory_field_kib(text: &str, field: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        line.strip_prefix(field)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
    })
}

#[cfg(target_os = "linux")]
struct ProcessMemoryKib {
    resident: u64,
    peak: u64,
}

#[cfg(target_os = "linux")]
const MEMORY_PROBE_SUMMARY_PREFIX: &str = "gh372-memory-probe-summary:";

#[cfg(target_os = "linux")]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct MailboxMemoryMeasurement {
    connections: usize,
    baseline_rss_kib: u64,
    initial_peak_kib: u64,
    final_rss_kib: u64,
    peak_kib: u64,
}

#[cfg(target_os = "linux")]
impl MailboxMemoryMeasurement {
    fn growth_kib(&self) -> u64 {
        self.peak_kib.saturating_sub(self.baseline_rss_kib)
    }
}

#[cfg(target_os = "linux")]
fn parse_memory_probe_summary(stderr: &[u8], connections: usize) -> MailboxMemoryMeasurement {
    let stderr = String::from_utf8_lossy(stderr);
    let mut summaries = stderr
        .lines()
        .filter_map(|line| line.strip_prefix(MEMORY_PROBE_SUMMARY_PREFIX));
    let measurement: MailboxMemoryMeasurement = serde_json::from_str(
        summaries
            .next()
            .expect("memory child must report its raw RSS and high-water measurements"),
    )
    .expect("valid memory measurement summary");
    assert!(
        summaries.next().is_none(),
        "memory child must report exactly one measurement summary"
    );
    assert_eq!(measurement.connections, connections);
    assert!(measurement.baseline_rss_kib > 0);
    assert!(measurement.final_rss_kib > 0);
    assert!(measurement.peak_kib > 0);
    measurement
}

#[cfg(target_os = "linux")]
fn log_memory_probe_environment() {
    // Inspect configuration in the parent, outside both children's baselines.
    // Preserve inherited options instead of changing the caller's allocator.
    let mut allocator_environment: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| {
            name.to_str().is_some_and(|name| {
                name.starts_with("MIMALLOC_")
                    || name.starts_with("mimalloc_")
                    || matches!(
                        name,
                        "MALLOC_ARENA_MAX" | "MALLOC_CONF" | "_RJEM_MALLOC_CONF"
                    )
            })
        })
        .collect();
    allocator_environment.sort_unstable();
    eprintln!("GH372 allocator_env={allocator_environment:?}");
    for path in [
        "/sys/kernel/mm/transparent_hugepage/enabled",
        "/sys/kernel/mm/transparent_hugepage/defrag",
        "/sys/kernel/mm/transparent_hugepage/hpage_pmd_size",
    ] {
        let policy = std::fs::read_to_string(path).map(|text| text.trim().to_owned());
        eprintln!("GH372 host_policy path={path} value={policy:?}");
    }
}

/// RSS includes resident worker stacks, allocator-retained pages, and file
/// mappings. PSS apportions shared mappings and helps distinguish those costs
/// from private memory; it is diagnostic only, never a substitute for the gate.
#[cfg(target_os = "linux")]
fn process_memory_kib(connections: usize, stage: &str) -> ProcessMemoryKib {
    let status = std::fs::read_to_string("/proc/self/status")
        .expect("Linux memory accounting is required for the memory regression");
    let required = |field| {
        memory_field_kib(&status, field)
            .unwrap_or_else(|| panic!("missing {field} in /proc/self/status"))
    };
    let snapshot = ProcessMemoryKib {
        resident: required("VmRSS:"),
        peak: required("VmHWM:"),
    };
    let anon = memory_field_kib(&status, "RssAnon:");
    let file = memory_field_kib(&status, "RssFile:");
    let shmem = memory_field_kib(&status, "RssShmem:");
    // Older kernels can lack smaps_rollup. Keep the existing RSS requirement
    // while making the more precise page-table accounting available when present.
    let smaps = std::fs::read_to_string("/proc/self/smaps_rollup").ok();
    let rollup = |field| {
        smaps
            .as_deref()
            .and_then(|text| memory_field_kib(text, field))
    };
    eprintln!(
        "GH372 connections={connections} stage={stage} rss_kib={} peak_kib={} \
         anon_kib={anon:?} file_kib={file:?} shmem_kib={shmem:?} \
         smaps_rss_kib={:?} pss_kib={:?} private_dirty_kib={:?} anon_huge_pages_kib={:?}",
        snapshot.resident,
        snapshot.peak,
        rollup("Rss:"),
        rollup("Pss:"),
        rollup("Private_Dirty:"),
        rollup("AnonHugePages:"),
    );
    snapshot
}

#[cfg(target_os = "linux")]
fn seed_legacy_memory_mailbox(path: &Path, message_count: i64) -> String {
    use mcp_agent_mail_db::sqlmodel::Value;

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
    for id in 1..=message_count {
        seed.execute_sync(
            "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts) \
             VALUES (?, 1, 1, 'before upgrade', ?, ?)",
            &[
                Value::BigInt(id),
                Value::Text(body.clone()),
                Value::BigInt(id),
            ],
        )
        .unwrap();
    }
    seed.execute_raw("COMMIT").unwrap();
    body
}

#[cfg(target_os = "linux")]
fn measure_existing_mailbox_connections(
    path: &Path,
    connection_count: usize,
) -> MailboxMemoryMeasurement {
    use mcp_agent_mail_db::sqlmodel::Value;

    assert!(matches!(connection_count, 1 | 5));
    assert!(path.is_absolute());
    assert_eq!(std::fs::canonicalize(path).unwrap().as_path(), path);
    assert!(
        path.parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("am-gh372-memory-"))
    );
    let baseline = process_memory_kib(connection_count, "baseline");
    let writer = DbConn::open_file(path.to_str().unwrap()).expect("open existing mailbox");
    process_memory_kib(connection_count, "writer_open");
    let mut readers = Vec::with_capacity(connection_count - 1);
    for index in 1..connection_count {
        readers.push(DbConn::open_file(path.to_str().unwrap()).expect("open runtime reader"));
        process_memory_kib(connection_count, &format!("reader_{index}_open"));
    }
    writer
        .execute_raw("UPDATE messages SET subject = 'post-upgrade write' WHERE id = 1")
        .expect("upgraded writer must preserve the existing mailbox format");
    process_memory_kib(connection_count, "committed_write");
    // Both children execute the same five query pairs. The single connection
    // repeats the writer read; the other child reads on its writer and readers.
    for index in 0..5 {
        let reader = if connection_count == 1 || index == 0 {
            &writer
        } else {
            &readers[index - 1]
        };
        assert_eq!(
            reader.query_sync("SELECT 1", &[]).unwrap()[0]
                .get_as::<i64>(0)
                .unwrap(),
            1
        );
        let row = reader
            .query_sync(
                "SELECT subject, body_md FROM messages WHERE id = ?",
                &[Value::BigInt(1)],
            )
            .expect("read committed message through another connection");
        assert_eq!(row[0].get_as::<String>(0).unwrap(), "post-upgrade write");
        assert_eq!(
            row[0].get_as::<String>(1).unwrap(),
            "retained body ".repeat(320)
        );
        drop(row);
        process_memory_kib(connection_count, &format!("connection_{index}_read"));
    }
    let final_memory = process_memory_kib(connection_count, "before_close");
    let measurement = MailboxMemoryMeasurement {
        connections: connection_count,
        baseline_rss_kib: baseline.resident,
        initial_peak_kib: baseline.peak,
        final_rss_kib: final_memory.resident,
        peak_kib: final_memory.peak,
    };
    eprintln!(
        "GH372 connections={connection_count} delta_kib={} original_32_mib_budget_passed={}",
        measurement.growth_kib(),
        measurement.growth_kib() <= 32 * 1024
    );
    // Dropping an async connection detaches its worker. Join every reader so
    // the diagnostic below observes completed teardown, without checkpointing
    // once per reader or moving any teardown work inside the measured interval.
    for reader in readers {
        reader
            .close_without_checkpoint_sync()
            .expect("join bounded-memory fixture reader");
    }
    writer
        .close_sync()
        .expect("close and flush bounded-memory fixture");
    process_memory_kib(connection_count, "after_close");
    measurement
}

#[cfg(target_os = "linux")]
fn verify_memory_mailbox(path: &Path, message_count: i64, body: &str) {
    use mcp_agent_mail_db::sqlmodel::Value;

    let canonical = CanonicalDbConn::open_file(path.to_str().unwrap()).unwrap();
    let rows = canonical
        .query_sync("SELECT COUNT(*), SUM(length(body_md)) FROM messages", &[])
        .unwrap();
    assert_eq!(rows[0].get_as::<i64>(0).unwrap(), message_count);
    assert_eq!(
        rows[0].get_as::<i64>(1).unwrap(),
        message_count * i64::try_from(body.len()).unwrap()
    );
    let preserved = canonical
        .query_sync(
            "SELECT COUNT(*) FROM messages WHERE body_md = ? AND subject = \
             CASE WHEN id = 1 THEN 'post-upgrade write' ELSE 'before upgrade' END",
            &[Value::Text(body.to_owned())],
        )
        .unwrap();
    assert_eq!(
        preserved[0].get_as::<i64>(0).unwrap(),
        message_count,
        "every body and subject must be preserved except for the intended update"
    );
    let integrity = canonical.query_sync("PRAGMA integrity_check", &[]).unwrap();
    assert_eq!(integrity.len(), 1);
    assert_eq!(integrity[0].get_as::<String>(0).unwrap(), "ok");
    assert!(
        canonical
            .query_sync("PRAGMA foreign_key_check", &[])
            .unwrap()
            .is_empty()
    );
    let updated = canonical
        .query_sync("SELECT subject FROM messages WHERE id = 1", &[])
        .unwrap();
    assert_eq!(
        updated[0].get_as::<String>(0).unwrap(),
        "post-upgrade write"
    );
}

/// GH#372 / br-kp1in.46: bound memory for small reads on an existing mailbox.
/// Compare one and five live connections on byte-identical fixtures in fresh
/// processes. Keep every child's original `VmHWM - baseline VmRSS` measurement;
/// the initial high-water mark and mapping diagnostics never replace that metric.
#[cfg(target_os = "linux")]
#[test]
fn existing_mailbox_reads_have_bounded_connection_memory() {
    const CHILD_ENV: &str = "AM_GH372_MEMORY_FIXTURE";
    const CONNECTIONS_ENV: &str = "AM_GH372_MEMORY_CONNECTIONS";
    const CHILD_TEST: &str =
        "startup_page_leak::existing_mailbox_reads_have_bounded_connection_memory";
    const ACK: &str = "gh372-memory-probe-complete";
    const MESSAGE_COUNT: i64 = 4096;
    if let Some(path) = std::env::var_os(CHILD_ENV) {
        let connection_count = std::env::var(CONNECTIONS_ENV).map_or(5, |value| {
            value
                .parse::<usize>()
                .expect("memory probe connection count")
        });
        let measurement =
            measure_existing_mailbox_connections(&PathBuf::from(path), connection_count);
        eprintln!(
            "{MEMORY_PROBE_SUMMARY_PREFIX}{}",
            serde_json::to_string(&measurement).unwrap()
        );
        eprintln!("{ACK}:{connection_count}");
        return;
    }

    log_memory_probe_environment();
    let dir = tempfile::Builder::new()
        .prefix("am-gh372-memory-")
        .tempdir()
        .expect("private memory fixture directory");
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let pristine = root.join("pristine.sqlite3");
    let body = seed_legacy_memory_mailbox(&pristine, MESSAGE_COUNT);
    let pristine_bytes = std::fs::read(&pristine).unwrap();
    assert!(pristine_bytes.len() > 16 * 1024 * 1024);
    assert!(usize::try_from(MESSAGE_COUNT).unwrap() * body.len() > 16 * 1024 * 1024);
    let measurements = [1, 5].map(|connection_count| {
        // The seed connection is closed before either copy. Each fresh process
        // receives the same physical layout and data, before the committed update.
        let path = root.join(format!("storage-{connection_count}.sqlite3"));
        std::fs::copy(&pristine, &path).unwrap();
        assert!(
            std::fs::read(&path).unwrap() == pristine_bytes,
            "memory children must receive byte-identical pristine mailboxes"
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, &path)
            .env(CONNECTIONS_ENV, connection_count.to_string())
            .output()
            .expect("run isolated memory measurement");
        std::io::stdout().write_all(&output.stdout).unwrap();
        std::io::stderr().write_all(&output.stderr).unwrap();
        assert!(
            output.status.success(),
            "{connection_count}-connection memory child failed: {}",
            output.status
        );
        let ack = format!("{ACK}:{connection_count}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .filter(|line| *line == ack)
                .count(),
            1,
            "a zero-test child filter is not a successful memory regression"
        );
        let measurement = parse_memory_probe_summary(&output.stderr, connection_count);
        verify_memory_mailbox(&path, MESSAGE_COUNT, &body);
        measurement
    });
    assert!(std::fs::read(&pristine).unwrap() == pristine_bytes);
    let [one, five] = measurements;
    let additional_readers_kib = five.growth_kib().saturating_sub(one.growth_kib());
    eprintln!(
        "GH372 paired one_connection_kib={} five_connections_kib={} additional_readers_kib={additional_readers_kib}",
        one.growth_kib(),
        five.growth_kib()
    );

    // Native GH#372 controls found about 24 MiB of executable paging plus
    // initialization costs; even one connection exceeded a 32 MiB total cap.
    // Bound total startup growth by 64 MiB and use a separate 32 MiB budget
    // for adding four readers. This calibration is for the fixed 17.5 MiB body
    // fixture; four body copies would add 70 MiB. Arbitrary database sizes need
    // separate qualification. Both gates include all resident-memory categories;
    // never subtract current mapping RSS from a historical high-water mark.
    // See UPGRADE_LOG.md for the paired allocator controls and qualification limits.
    for measurement in [&one, &five] {
        assert!(
            measurement.growth_kib() <= 64 * 1024,
            "{} connections exceeded 64 MiB total RSS growth: {measurement:?}",
            measurement.connections
        );
    }
    assert!(
        additional_readers_kib <= 32 * 1024,
        "four additional readers exceeded 32 MiB RSS growth: one={one:?}, five={five:?}"
    );
}
