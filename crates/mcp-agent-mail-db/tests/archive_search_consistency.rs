//! Archive/search consistency differential corpus.
//!
//! These tests intentionally use deterministic archive and salvage fixtures
//! (`R1` in `docs/VERIFICATION_COVERAGE_LEDGER.md`) while exercising the real
//! reconstruction, `SQLite` query, schema invariant, Search V3 lexical health,
//! and product-bus search paths.

#![allow(
    clippy::too_many_lines,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::similar_names
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use asupersync::{Cx, Outcome};
use mcp_agent_mail_db::search_service::{LexicalBackfillHealth, lexical_backfill_health};
use mcp_agent_mail_db::{
    DbConn, DbError, DbPool, DbPoolConfig, ReconstructStats, check_schema_invariants_conn, queries,
    reconstruct_from_archive_with_salvage, scan_archive_message_inventory,
};
use serde::Serialize;
use serde_json::json;

const PRODUCT_UID: &str = "archive-search-consistency";
const PRODUCT_QUERY: &str = "consistency";
const EXPECTED_PROJECTS: usize = 3;
const EXPECTED_AGENTS: usize = 6;
const EXPECTED_MESSAGES: usize = 4;
const EXPECTED_RECIPIENTS: usize = 5;
const EXPECTED_FILE_RESERVATIONS: usize = 1;
const EXPECTED_AGENT_LINKS: usize = 1;
const EXPECTED_PRODUCT_LINKS: usize = 2;

struct CorpusFixture {
    _tmp: tempfile::TempDir,
    storage_root: PathBuf,
    db_path: PathBuf,
    pool: DbPool,
    product_id: i64,
    reconstruct_stats: ReconstructStats,
}

#[derive(Debug, Serialize)]
struct CountSnapshot {
    projects: usize,
    agents: usize,
    messages: usize,
    recipients: usize,
    file_reservations: usize,
    agent_links: usize,
    product_links: usize,
}

#[derive(Debug, Serialize)]
struct Mismatch {
    kind: &'static str,
    entity: &'static str,
    expected: String,
    actual: String,
    detail: String,
}

#[derive(Debug, Serialize)]
struct ProductScopeReport {
    product_uid: &'static str,
    query: &'static str,
    expected_project_slugs: Vec<String>,
    actual_project_slugs: Vec<String>,
    result_subjects: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ReconstructStatsSnapshot {
    projects: usize,
    agents: usize,
    messages: usize,
    recipients: usize,
    duplicate_canonical_message_files: usize,
    duplicate_canonical_message_ids: usize,
    cross_project_canonical_collisions: usize,
    salvaged_projects: usize,
    salvaged_agents: usize,
    salvaged_messages: usize,
    salvaged_recipients: usize,
    rollups_salvaged: usize,
    parse_errors: usize,
    warnings: Vec<String>,
}

impl From<&ReconstructStats> for ReconstructStatsSnapshot {
    fn from(stats: &ReconstructStats) -> Self {
        Self {
            projects: stats.projects,
            agents: stats.agents,
            messages: stats.messages,
            recipients: stats.recipients,
            duplicate_canonical_message_files: stats.duplicate_canonical_message_files,
            duplicate_canonical_message_ids: stats.duplicate_canonical_message_ids,
            cross_project_canonical_collisions: stats.cross_project_canonical_collisions,
            salvaged_projects: stats.salvaged_projects,
            salvaged_agents: stats.salvaged_agents,
            salvaged_messages: stats.salvaged_messages,
            salvaged_recipients: stats.salvaged_recipients,
            rollups_salvaged: stats.rollups_salvaged,
            parse_errors: stats.parse_errors,
            warnings: stats.warnings.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ConsistencyReport {
    schema_version: u32,
    scenario: &'static str,
    coverage_grade: &'static str,
    coverage_note: &'static str,
    intentional_divergence: &'static str,
    archive_counts: CountSnapshot,
    db_counts: CountSnapshot,
    reconstruct_stats: ReconstructStatsSnapshot,
    lexical_backfill: LexicalBackfillHealth,
    product_scope: ProductScopeReport,
    mismatches: Vec<Mismatch>,
}

impl ConsistencyReport {
    fn assert_clean(&self) {
        assert!(
            self.mismatches.is_empty(),
            "archive/search consistency mismatches for {}:\n{}",
            self.scenario,
            serde_json::to_string_pretty(self).expect("serialize report")
        );
    }

    fn assert_has_mismatch(&self, kind: &str) {
        assert!(
            self.mismatches.iter().any(|mismatch| mismatch.kind == kind),
            "expected mismatch kind {kind}, got:\n{}",
            serde_json::to_string_pretty(self).expect("serialize report")
        );
    }
}

fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    common::block_on(f)
}

fn expect_outcome<T>(outcome: Outcome<T, DbError>, context: &str) -> T {
    match outcome {
        Outcome::Ok(value) => value,
        Outcome::Err(error) => panic!("{context} failed: {error:?}"),
        Outcome::Cancelled(reason) => panic!("{context} cancelled: {reason:?}"),
        Outcome::Panicked(panic) => panic!("{context} panicked: {panic:?}"),
    }
}

fn expected_product_project_slugs() -> BTreeSet<String> {
    ["consistency-alpha", "consistency-beta"]
        .into_iter()
        .map(String::from)
        .collect()
}

fn setup_consistency_fixture() -> CorpusFixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage_root = tmp.path().join("storage");
    let db_path = tmp.path().join("reconstructed.sqlite3");
    let salvage_db_path = tmp.path().join("salvage.sqlite3");

    build_archive_fixture(&storage_root);
    build_salvage_fixture(&salvage_db_path);

    let reconstruct_stats =
        reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
            .expect("reconstruct from archive with product/contact salvage");

    let pool = DbPool::new(&DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        storage_root: Some(storage_root.clone()),
        max_connections: 4,
        min_connections: 1,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: false,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    })
    .expect("create consistency fixture pool");
    let product_id = scalar_i64(
        &open_conn(&db_path),
        "SELECT id AS n FROM products WHERE product_uid = 'archive-search-consistency'",
    );

    CorpusFixture {
        _tmp: tmp,
        storage_root,
        db_path,
        pool,
        product_id,
        reconstruct_stats,
    }
}

fn open_conn(db_path: &Path) -> DbConn {
    DbConn::open_file(db_path.display().to_string()).expect("open fixture database")
}

fn scalar_i64(conn: &DbConn, sql: &str) -> i64 {
    conn.query_sync(sql, &[])
        .expect("scalar query")
        .first()
        .unwrap_or_else(|| panic!("scalar query returned no rows: {sql}"))
        .get_named::<i64>("n")
        .expect("scalar column n")
}

fn write_project_metadata(storage_root: &Path, slug: &str, human_key: &str) {
    let project_dir = storage_root.join("projects").join(slug);
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    let metadata = json!({
        "slug": slug,
        "human_key": human_key,
        "created_at": 0,
    });
    std::fs::write(
        project_dir.join("project.json"),
        serde_json::to_string_pretty(&metadata).expect("serialize project metadata"),
    )
    .expect("write project metadata");
}

fn write_agent_profile(storage_root: &Path, project_slug: &str, agent_name: &str) {
    let agent_dir = storage_root
        .join("projects")
        .join(project_slug)
        .join("agents")
        .join(agent_name);
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    let profile = json!({
        "name": agent_name,
        "program": "codex-cli",
        "model": "gpt-5.5",
        "task_description": "archive/search consistency fixture",
        "inception_ts": "2026-05-13T00:00:00Z",
        "last_active_ts": "2026-05-13T00:00:00Z",
        "attachments_policy": "auto",
        "contact_policy": "auto",
    });
    std::fs::write(
        agent_dir.join("profile.json"),
        serde_json::to_string_pretty(&profile).expect("serialize profile"),
    )
    .expect("write profile");
}

fn write_archive_message(
    storage_root: &Path,
    project_slug: &str,
    filename: &str,
    frontmatter: &serde_json::Value,
    body: &str,
) {
    let message_dir = storage_root
        .join("projects")
        .join(project_slug)
        .join("messages")
        .join("2026")
        .join("05");
    std::fs::create_dir_all(&message_dir).expect("create message dir");
    let frontmatter_text =
        serde_json::to_string_pretty(&frontmatter).expect("serialize frontmatter");
    std::fs::write(
        message_dir.join(filename),
        format!("---json\n{frontmatter_text}\n---\n\n{body}\n"),
    )
    .expect("write archive message");
}

fn write_file_reservation(storage_root: &Path) {
    let reservation_dir = storage_root
        .join("projects")
        .join("consistency-alpha")
        .join("file_reservations");
    std::fs::create_dir_all(&reservation_dir).expect("create reservation dir");
    let reservation = json!({
        "id": 9001,
        "project": "/tmp/archive-search/alpha",
        "agent": "AlphaSender",
        "path_pattern": "crates/mcp-agent-mail-db/tests/archive_search_consistency.rs",
        "exclusive": true,
        "reason": "archive/search consistency fixture",
        "created_ts": "2026-05-13T00:00:00Z",
        "expires_ts": "2026-05-13T02:00:00Z",
    });
    std::fs::write(
        reservation_dir.join("id-9001.json"),
        serde_json::to_string_pretty(&reservation).expect("serialize reservation"),
    )
    .expect("write reservation");
}

fn build_archive_fixture(storage_root: &Path) {
    for (slug, human_key, agents) in [
        (
            "consistency-alpha",
            "/tmp/archive-search/alpha",
            ["AlphaSender", "AlphaReviewer"],
        ),
        (
            "consistency-beta",
            "/tmp/archive-search/beta",
            ["BetaSender", "BetaReviewer"],
        ),
        (
            "consistency-gamma",
            "/tmp/archive-search/gamma",
            ["GammaSender", "GammaReviewer"],
        ),
    ] {
        write_project_metadata(storage_root, slug, human_key);
        for agent in agents {
            write_agent_profile(storage_root, slug, agent);
        }
    }

    write_archive_message(
        storage_root,
        "consistency-alpha",
        "2026-05-13T00-00-00Z__alpha-plan__1001.md",
        &json!({
            "id": 1001,
            "from": "AlphaSender",
            "to": ["AlphaReviewer"],
            "cc": ["AlphaSender"],
            "bcc": [],
            "thread_id": "CONSISTENCY-1",
            "subject": "Consistency atlas alpha plan",
            "importance": "high",
            "ack_required": true,
            "created_ts": "2026-05-13T00:00:00Z",
            "attachments": [{"name": "diagram.txt", "path": "attachments/diagram.txt"}],
        }),
        "Alpha project archive consistency needle with repair reconstruct backfill coverage.",
    );
    write_archive_message(
        storage_root,
        "consistency-alpha",
        "2026-05-13T00-05-00Z__alpha-reply__1002.md",
        &json!({
            "id": 1002,
            "from": "AlphaReviewer",
            "to": ["AlphaSender"],
            "cc": [],
            "bcc": [],
            "thread_id": "CONSISTENCY-1",
            "subject": "Consistency atlas alpha reply",
            "importance": "normal",
            "ack_required": false,
            "created_ts": "2026-05-13T00:05:00Z",
            "attachments": [],
        }),
        "Alpha reply keeps the thread reply path in the corpus.",
    );
    write_archive_message(
        storage_root,
        "consistency-beta",
        "2026-05-13T00-10-00Z__beta-plan__2001.md",
        &json!({
            "id": 2001,
            "from": "BetaSender",
            "to": ["BetaReviewer"],
            "cc": [],
            "bcc": [],
            "thread_id": "CONSISTENCY-2",
            "subject": "Consistency atlas beta plan",
            "importance": "normal",
            "ack_required": false,
            "created_ts": "2026-05-13T00:10:00Z",
            "attachments": [],
        }),
        "Beta project archive consistency needle for product-bus cross-project search.",
    );
    write_archive_message(
        storage_root,
        "consistency-gamma",
        "2026-05-13T00-20-00Z__gamma-private__3001.md",
        &json!({
            "id": 3001,
            "from": "GammaSender",
            "to": ["GammaReviewer"],
            "cc": [],
            "bcc": [],
            "thread_id": "CONSISTENCY-3",
            "subject": "Consistency atlas gamma private",
            "importance": "urgent",
            "ack_required": false,
            "created_ts": "2026-05-13T00:20:00Z",
            "attachments": [],
        }),
        "Gamma project archive consistency needle must not leak into product results.",
    );

    write_file_reservation(storage_root);
}

fn build_salvage_fixture(db_path: &Path) {
    let conn = DbConn::open_file(db_path.display().to_string()).expect("open salvage database");
    conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
        .expect("init salvage schema");
    conn.execute_raw(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES
            (10, 'consistency-alpha', '/tmp/archive-search/alpha', 1),
            (20, 'consistency-beta', '/tmp/archive-search/beta', 2),
            (30, 'consistency-gamma', '/tmp/archive-search/gamma', 3)",
    )
    .expect("insert salvage projects");
    conn.execute_raw(
        "INSERT INTO agents (
            id, project_id, name, program, model, task_description,
            inception_ts, last_active_ts, attachments_policy, contact_policy
        ) VALUES
            (101, 10, 'AlphaSender', 'codex-cli', 'gpt-5.5', '', 1, 2, 'auto', 'auto'),
            (102, 10, 'AlphaReviewer', 'codex-cli', 'gpt-5.5', '', 1, 2, 'auto', 'auto'),
            (201, 20, 'BetaSender', 'codex-cli', 'gpt-5.5', '', 1, 2, 'auto', 'auto'),
            (202, 20, 'BetaReviewer', 'codex-cli', 'gpt-5.5', '', 1, 2, 'auto', 'auto'),
            (301, 30, 'GammaSender', 'codex-cli', 'gpt-5.5', '', 1, 2, 'auto', 'auto'),
            (302, 30, 'GammaReviewer', 'codex-cli', 'gpt-5.5', '', 1, 2, 'auto', 'auto')",
    )
    .expect("insert salvage agents");
    conn.execute_raw(
        "INSERT INTO agent_links (
            a_project_id, a_agent_id, b_project_id, b_agent_id,
            status, reason, created_ts, updated_ts, expires_ts
        ) VALUES (
            10, 101, 20, 202, 'approved', 'cross-project consistency contact', 3, 4, 5
        )",
    )
    .expect("insert salvage contact");
    conn.execute_raw(
        "INSERT INTO products (id, product_uid, name, created_at)
         VALUES (700, 'archive-search-consistency', 'Archive Search Consistency', 6)",
    )
    .expect("insert salvage product");
    conn.execute_raw(
        "INSERT INTO product_project_links (product_id, project_id, created_at) VALUES
            (700, 10, 7),
            (700, 20, 8)",
    )
    .expect("insert salvage product links");
}

fn write_lexical_backfill_state(pool: &DbPool, storage_root: &Path, indexed_count: usize) {
    let conn = open_conn(Path::new(pool.sqlite_path()));
    let db_count = scalar_i64(&conn, "SELECT COUNT(*) AS n FROM messages") as u64;
    let db_max_id = scalar_i64(&conn, "SELECT COALESCE(MAX(id), 0) AS n FROM messages") as u64;
    let index_dir = storage_root.join("search_index");
    std::fs::create_dir_all(&index_dir).expect("create search index dir");
    let payload = json!({
        "schema_version": 1,
        "db_path": pool.sqlite_path(),
        "db_stats": {
            "count": db_count,
            "max_id": db_max_id,
        },
        "index_stats": {
            "count": indexed_count as u64,
            "max_id": if indexed_count == db_count as usize {
                db_max_id
            } else {
                db_max_id.saturating_sub(1)
            },
        },
        "updated_at_micros": 1_778_627_200_000_000_i64,
    });
    std::fs::write(
        index_dir.join("backfill_state.json"),
        serde_json::to_string_pretty(&payload).expect("serialize backfill state"),
    )
    .expect("write backfill state");
}

fn table_count(conn: &DbConn, table: &str) -> usize {
    scalar_i64(conn, &format!("SELECT COUNT(*) AS n FROM {table}")) as usize
}

fn db_counts(conn: &DbConn) -> CountSnapshot {
    CountSnapshot {
        projects: table_count(conn, "projects"),
        agents: table_count(conn, "agents"),
        messages: table_count(conn, "messages"),
        recipients: table_count(conn, "message_recipients"),
        file_reservations: table_count(conn, "file_reservations"),
        agent_links: table_count(conn, "agent_links"),
        product_links: table_count(conn, "product_project_links"),
    }
}

fn archive_counts(storage_root: &Path) -> CountSnapshot {
    let inventory = scan_archive_message_inventory(storage_root);
    CountSnapshot {
        projects: inventory.projects,
        agents: inventory.agents,
        messages: inventory.unique_message_ids,
        recipients: EXPECTED_RECIPIENTS,
        file_reservations: EXPECTED_FILE_RESERVATIONS,
        agent_links: 0,
        product_links: 0,
    }
}

fn project_slugs_by_id(conn: &DbConn) -> BTreeMap<i64, String> {
    conn.query_sync("SELECT id, slug FROM projects ORDER BY id", &[])
        .expect("query project slugs")
        .into_iter()
        .map(|row| {
            (
                row.get_named::<i64>("id").expect("project id"),
                row.get_named::<String>("slug").expect("project slug"),
            )
        })
        .collect()
}

fn product_scope_report(
    pool: &DbPool,
    product_id: i64,
    expected_project_slugs: &BTreeSet<String>,
) -> ProductScopeReport {
    let rows = block_on(|cx| {
        let pool = pool.clone();
        async move {
            expect_outcome(
                queries::search_messages_for_product(&cx, &pool, product_id, PRODUCT_QUERY, 50)
                    .await,
                "search_messages_for_product",
            )
        }
    });
    let conn = open_conn(Path::new(pool.sqlite_path()));
    let slugs_by_id = project_slugs_by_id(&conn);
    let actual_project_slugs = rows
        .iter()
        .filter_map(|row| slugs_by_id.get(&row.project_id).cloned())
        .collect::<BTreeSet<_>>();
    ProductScopeReport {
        product_uid: PRODUCT_UID,
        query: PRODUCT_QUERY,
        expected_project_slugs: expected_project_slugs.iter().cloned().collect(),
        actual_project_slugs: actual_project_slugs.into_iter().collect(),
        result_subjects: rows.into_iter().map(|row| row.subject).collect(),
    }
}

fn push_count_mismatch(
    mismatches: &mut Vec<Mismatch>,
    kind: &'static str,
    entity: &'static str,
    expected: usize,
    actual: usize,
    detail: &str,
) {
    if expected != actual {
        mismatches.push(Mismatch {
            kind,
            entity,
            expected: expected.to_string(),
            actual: actual.to_string(),
            detail: detail.to_string(),
        });
    }
}

fn build_consistency_report(
    fixture: &CorpusFixture,
    scenario: &'static str,
    expected_project_slugs: &BTreeSet<String>,
) -> ConsistencyReport {
    let conn = open_conn(&fixture.db_path);
    let archive_counts = archive_counts(&fixture.storage_root);
    let db_counts = db_counts(&conn);
    let lexical_backfill = lexical_backfill_health(&fixture.pool);
    let product_scope =
        product_scope_report(&fixture.pool, fixture.product_id, expected_project_slugs);
    let mut mismatches = Vec::new();

    push_count_mismatch(
        &mut mismatches,
        "archive_db_count_drift",
        "messages",
        archive_counts.messages,
        db_counts.messages,
        "archive message files and database message rows must agree after reconstruct",
    );
    push_count_mismatch(
        &mut mismatches,
        "missing_recipient_rows",
        "message_recipients",
        EXPECTED_RECIPIENTS,
        db_counts.recipients,
        "recipient rows are reconstructed from archive to/cc/bcc frontmatter",
    );
    push_count_mismatch(
        &mut mismatches,
        "archive_db_count_drift",
        "projects",
        EXPECTED_PROJECTS,
        db_counts.projects,
        "project metadata from archive must match database project rows",
    );
    push_count_mismatch(
        &mut mismatches,
        "archive_db_count_drift",
        "agents",
        EXPECTED_AGENTS,
        db_counts.agents,
        "agent profiles from archive must match database agent rows",
    );
    push_count_mismatch(
        &mut mismatches,
        "archive_db_count_drift",
        "file_reservations",
        EXPECTED_FILE_RESERVATIONS,
        db_counts.file_reservations,
        "file reservation artifacts must be replayed into the database",
    );
    push_count_mismatch(
        &mut mismatches,
        "contact_salvage_drift",
        "agent_links",
        EXPECTED_AGENT_LINKS,
        db_counts.agent_links,
        "DB-only contact state must survive salvage-backed reconstruction",
    );
    push_count_mismatch(
        &mut mismatches,
        "product_scope_drift",
        "product_project_links",
        EXPECTED_PRODUCT_LINKS,
        db_counts.product_links,
        "DB-only product links must survive salvage-backed reconstruction",
    );

    if lexical_backfill.state != "fresh"
        || lexical_backfill.source_messages != Some(db_counts.messages as u64)
        || lexical_backfill.indexed_messages != db_counts.messages as u64
    {
        mismatches.push(Mismatch {
            kind: "stale_search_generation",
            entity: "search_index",
            expected: format!("fresh/{}", db_counts.messages),
            actual: format!(
                "{}/{}",
                lexical_backfill.state, lexical_backfill.indexed_messages
            ),
            detail: lexical_backfill
                .stale_reason
                .clone()
                .unwrap_or_else(|| "lexical backfill health is not fresh".to_string()),
        });
    }

    let expected = expected_project_slugs
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let actual = product_scope
        .actual_project_slugs
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let leaked = actual.difference(&expected).cloned().collect::<Vec<_>>();
    let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
    if !leaked.is_empty() {
        mismatches.push(Mismatch {
            kind: "product_scope_leak",
            entity: "search_messages_for_product",
            expected: format!("{expected:?}"),
            actual: format!("{actual:?}"),
            detail: format!("unlinked project slugs appeared in product search: {leaked:?}"),
        });
    }
    if !missing.is_empty() {
        mismatches.push(Mismatch {
            kind: "product_scope_missing",
            entity: "search_messages_for_product",
            expected: format!("{expected:?}"),
            actual: format!("{actual:?}"),
            detail: format!("linked project slugs missing from product search: {missing:?}"),
        });
    }

    match check_schema_invariants_conn(&conn) {
        Ok(report) if report.is_healthy() => {}
        Ok(report) => mismatches.push(Mismatch {
            kind: "schema_invariant_drift",
            entity: "schema_invariants",
            expected: "healthy".to_string(),
            actual: format!("{:?}", report.findings),
            detail: "schema invariant checker reported relational drift".to_string(),
        }),
        Err(error) => mismatches.push(Mismatch {
            kind: "schema_invariant_drift",
            entity: "schema_invariants",
            expected: "healthy".to_string(),
            actual: error.to_string(),
            detail: "schema invariant checker failed".to_string(),
        }),
    }

    ConsistencyReport {
        schema_version: 1,
        scenario,
        coverage_grade: "R1 Deterministic local fixture",
        coverage_note: "Real reconstruct, SQLite, schema invariant, product query, and lexical-health paths over synthetic archive/salvage fixtures; see docs/VERIFICATION_COVERAGE_LEDGER.md.",
        intentional_divergence: "Archive-only reconstruction intentionally lacks contact and product-bus rows; this corpus uses salvage DB rows as the DB-only source of truth for those surfaces.",
        archive_counts,
        db_counts,
        reconstruct_stats: ReconstructStatsSnapshot::from(&fixture.reconstruct_stats),
        lexical_backfill,
        product_scope,
        mismatches,
    }
}

fn write_report_artifact(report: &ConsistencyReport) {
    let dir = Path::new("tests/artifacts/archive_search_consistency");
    std::fs::create_dir_all(dir).expect("create archive/search artifact dir");
    std::fs::write(
        dir.join(format!("{}_report.json", report.scenario)),
        serde_json::to_string_pretty(report).expect("serialize consistency report"),
    )
    .expect("write archive/search consistency report");
}

#[test]
fn archive_search_consistency_reconstruct_product_scope_report_is_clean() {
    let fixture = setup_consistency_fixture();
    write_lexical_backfill_state(&fixture.pool, &fixture.storage_root, EXPECTED_MESSAGES);

    let report = build_consistency_report(
        &fixture,
        "clean_reconstruct_product_scope",
        &expected_product_project_slugs(),
    );
    write_report_artifact(&report);

    report.assert_clean();
}

#[test]
fn archive_search_consistency_report_detects_missing_recipient_rows() {
    let fixture = setup_consistency_fixture();
    let conn = open_conn(&fixture.db_path);
    conn.execute_raw("DELETE FROM message_recipients WHERE message_id = 1001 AND kind = 'cc'")
        .expect("delete one recipient row");
    write_lexical_backfill_state(&fixture.pool, &fixture.storage_root, EXPECTED_MESSAGES);

    let report = build_consistency_report(
        &fixture,
        "missing_recipient_rows",
        &expected_product_project_slugs(),
    );
    write_report_artifact(&report);

    report.assert_has_mismatch("missing_recipient_rows");
}

#[test]
fn archive_search_consistency_report_detects_stale_search_generation() {
    let fixture = setup_consistency_fixture();
    write_lexical_backfill_state(&fixture.pool, &fixture.storage_root, EXPECTED_MESSAGES - 1);

    let report = build_consistency_report(
        &fixture,
        "stale_search_generation",
        &expected_product_project_slugs(),
    );
    write_report_artifact(&report);

    report.assert_has_mismatch("stale_search_generation");
}

#[test]
fn archive_search_consistency_report_detects_product_scope_leaks() {
    let fixture = setup_consistency_fixture();
    let conn = open_conn(&fixture.db_path);
    let gamma_project_id = scalar_i64(
        &conn,
        "SELECT id AS n FROM projects WHERE slug = 'consistency-gamma'",
    );
    conn.execute_raw(&format!(
        "INSERT OR IGNORE INTO product_project_links (product_id, project_id, created_at)
         VALUES ({}, {gamma_project_id}, 9)",
        fixture.product_id
    ))
    .expect("insert product scope leak");
    write_lexical_backfill_state(&fixture.pool, &fixture.storage_root, EXPECTED_MESSAGES);

    let report = build_consistency_report(
        &fixture,
        "product_scope_leak",
        &expected_product_project_slugs(),
    );
    write_report_artifact(&report);

    report.assert_has_mismatch("product_scope_leak");
}

#[test]
fn archive_search_consistency_report_detects_archive_db_count_drift() {
    let fixture = setup_consistency_fixture();
    let conn = open_conn(&fixture.db_path);
    conn.execute_raw("DELETE FROM message_recipients WHERE message_id = 3001")
        .expect("delete drift message recipient rows");
    conn.execute_raw("DELETE FROM messages WHERE id = 3001")
        .expect("delete drift message");
    write_lexical_backfill_state(&fixture.pool, &fixture.storage_root, EXPECTED_MESSAGES - 1);

    let report = build_consistency_report(
        &fixture,
        "archive_db_count_drift",
        &expected_product_project_slugs(),
    );
    write_report_artifact(&report);

    report.assert_has_mismatch("archive_db_count_drift");
}
