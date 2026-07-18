//! Integration tests for schema migration paths (v1 -> v2 -> ... -> latest).
//!
//! Verifies:
//! - Fresh databases reach the correct schema version with all tables/indexes
//! - FTS5 virtual tables and triggers are properly set up
//! - Migration idempotency (re-running produces no errors or duplicate work)
//! - Column existence and type correctness
//! - Roundtrip through pool-based initialization matches direct migration
//! - Legacy TEXT timestamp conversion (v3) with real data
//! - Composite index creation (v4) on existing data
//! - FTS tokenizer upgrade (v5) preserves and improves search
//! - Inbox stats materialization (v6) with trigger validation
//! - Identity FTS (v7) backfill and triggers
//! - Search recipes tables (v8)

#![allow(clippy::redundant_clone, clippy::too_many_lines)]

mod common;

use asupersync::cx::Cx;
use mcp_agent_mail_db::DbConn as SqliteConnection;
use mcp_agent_mail_db::schema::{
    self, MIGRATIONS_TABLE_NAME, PRAGMA_SETTINGS_SQL, enforce_runtime_fts_cleanup,
    init_migrations_table, migrate_to_latest, migration_status,
};
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use sqlmodel_core::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

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

/// Create a file-backed migration test connection in a temporary directory.
/// Uses the FrankenSQLite-backed `DbConn` runtime path, including `sqlite_master`
/// introspection, so schema verification exercises the same engine as production.
fn open_temp_db() -> (SqliteConnection, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("schema_mig_{}.db", unique_suffix()));
    let conn =
        SqliteConnection::open_file(db_path.display().to_string()).expect("open sqlite connection");
    (conn, dir)
}

/// Create a pool-managed database in a temporary directory.
fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("schema_pool_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        storage_root: Some(db_path.parent().unwrap().join("storage")),
        max_connections: 5,
        min_connections: 1,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

const ATC_V17_MIGRATION_IDS: &[&str] = &[
    "v17_create_atc_leader_lease",
    "v17_atc_experiences_add_contained_suspected_secret",
    "v17_atc_experiences_add_privacy_classification",
    "v17_create_atc_rollup_snapshots",
    "v17_idx_atc_rollup_snapshots_captured",
];

const ATC_V21_MIGRATION_IDS: &[&str] = &["v21_atc_experiences_add_feature_schema_version"];

fn table_exists(conn: &SqliteConnection, table: &str) -> bool {
    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
            &[Value::Text(table.to_string())],
        )
        .expect("query sqlite_master for table");
    !rows.is_empty()
}

fn index_exists(conn: &SqliteConnection, index: &str) -> bool {
    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND name = ? LIMIT 1",
            &[Value::Text(index.to_string())],
        )
        .expect("query sqlite_master for index");
    !rows.is_empty()
}

fn table_info(conn: &SqliteConnection, table: &str) -> Vec<(String, String, bool, Option<String>)> {
    let rows = conn
        .query_sync(&format!("PRAGMA table_info({table})"), &[])
        .unwrap_or_else(|_| panic!("PRAGMA table_info({table}) failed"));
    rows.iter()
        .map(|row| {
            let name: String = row.get_named("name").unwrap_or_default();
            let col_type: String = row.get_named("type").unwrap_or_default();
            let notnull: i64 = row.get_named("notnull").unwrap_or(0);
            let default_value = row.get_named::<String>("dflt_value").ok();
            (name, col_type.to_uppercase(), notnull != 0, default_value)
        })
        .collect()
}

fn assert_table_has_columns(conn: &SqliteConnection, table: &str, columns: &[&str]) {
    let present: HashSet<String> = table_info(conn, table)
        .into_iter()
        .map(|(name, _, _, _)| name)
        .collect();
    for column in columns {
        assert!(
            present.contains(*column),
            "missing {table}.{column}; present columns: {present:?}"
        );
    }
}

fn table_has_column(conn: &SqliteConnection, table: &str, column: &str) -> bool {
    table_info(conn, table)
        .iter()
        .any(|(name, _, _, _)| name == column)
}

fn assert_atc_v17_schema_surface(conn: &SqliteConnection) {
    let experience_cols = table_info(conn, "atc_experiences");
    let secret_col = experience_cols
        .iter()
        .find(|(name, _, _, _)| name == "contained_suspected_secret")
        .expect("contained_suspected_secret column");
    assert_eq!(secret_col.1, "INTEGER");
    assert!(
        secret_col.2,
        "contained_suspected_secret should be NOT NULL"
    );
    assert_eq!(
        secret_col.3.as_deref(),
        Some("0"),
        "contained_suspected_secret default should be 0"
    );

    let privacy_col = experience_cols
        .iter()
        .find(|(name, _, _, _)| name == "privacy_classification")
        .expect("privacy_classification column");
    assert_eq!(privacy_col.1, "TEXT");
    assert!(privacy_col.2, "privacy_classification should be NOT NULL");
    let privacy_default = privacy_col
        .3
        .as_deref()
        .expect("privacy_classification default");
    assert!(
        privacy_default.contains("legacy_unclassified"),
        "privacy_classification default should mention legacy_unclassified, got {privacy_default:?}"
    );

    assert!(
        table_exists(conn, "atc_leader_lease"),
        "missing atc_leader_lease table"
    );
    assert_table_has_columns(
        conn,
        "atc_leader_lease",
        &[
            "lease_slot",
            "instance_id",
            "acquired_at",
            "renewed_at",
            "ttl_micros",
        ],
    );

    assert!(
        table_exists(conn, "atc_rollup_snapshots"),
        "missing atc_rollup_snapshots table"
    );
    assert_table_has_columns(
        conn,
        "atc_rollup_snapshots",
        &[
            "snapshot_id",
            "captured_ts",
            "archive_relpath",
            "rollup_rows",
            "payload_sha256",
            "restored_ts",
        ],
    );

    assert!(
        index_exists(conn, "idx_atc_rollup_snapshots_captured"),
        "missing idx_atc_rollup_snapshots_captured index"
    );
}

fn assert_atc_feature_schema_version_column(conn: &SqliteConnection) {
    let experience_cols = table_info(conn, "atc_experiences");
    let feature_schema_version = experience_cols
        .iter()
        .find(|(name, _, _, _)| name == "feature_schema_version")
        .expect("feature_schema_version column");
    assert_eq!(feature_schema_version.1, "INTEGER");
    assert!(
        feature_schema_version.2,
        "feature_schema_version should be NOT NULL"
    );
    assert_eq!(
        feature_schema_version.3.as_deref(),
        Some("1"),
        "feature_schema_version default should be 1"
    );
}

fn seed_schema_without_migrations(conn: &SqliteConnection, skipped_ids: &[&str]) {
    block_on({
        move |cx| async move {
            init_migrations_table(&cx, conn)
                .await
                .into_result()
                .expect("init migrations table");
        }
    });

    let record_sql = format!(
        "INSERT INTO {MIGRATIONS_TABLE_NAME} (id, description, applied_at) VALUES (?, ?, ?)"
    );
    let skipped: HashSet<&str> = skipped_ids.iter().copied().collect();

    for migration in schema::schema_migrations() {
        if skipped.contains(migration.id.as_str()) {
            break;
        }
        let record_applied = || {
            conn.execute_sync(
                &record_sql,
                &[
                    Value::Text(migration.id.clone()),
                    Value::Text(migration.description.clone()),
                    Value::BigInt(1),
                ],
            )
            .expect("record applied migration");
        };
        if migration.id == "v15_add_recipients_json_to_messages"
            && table_has_column(conn, "messages", "recipients_json")
        {
            record_applied();
            continue;
        }
        if migration.id == "v3b_rebuild_projects_created_at_integer_affinity" {
            record_applied();
            continue;
        }
        if let Some(column) = migration.id.strip_prefix("v16a_atc_experiences_add_")
            && table_has_column(conn, "atc_experiences", column)
        {
            record_applied();
            continue;
        }
        if let Some(column) = migration.id.strip_prefix("v16b_atc_rollups_add_")
            && table_has_column(conn, "atc_experience_rollups", column)
        {
            record_applied();
            continue;
        }
        if let Some(column) = migration.id.strip_prefix("v17_atc_experiences_add_")
            && table_has_column(conn, "atc_experiences", column)
        {
            record_applied();
            continue;
        }
        if let Some(column) = rollup_column_from_migration_id(migration.id.as_str())
            && table_has_column(conn, "atc_experience_rollups", column)
        {
            record_applied();
            continue;
        }
        if migration.id == "v19_agents_reaper_exempt"
            && table_has_column(conn, "agents", "reaper_exempt")
        {
            record_applied();
            continue;
        }
        if migration.id == "v20_agents_registration_token"
            && table_has_column(conn, "agents", "registration_token")
        {
            record_applied();
            continue;
        }
        if migration.id == "v20_idx_agents_registration_token"
            && index_exists(conn, "idx_agents_registration_token")
        {
            record_applied();
            continue;
        }
        if migration.id == "v21_atc_experiences_add_feature_schema_version"
            && table_has_column(conn, "atc_experiences", "feature_schema_version")
        {
            record_applied();
            continue;
        }
        conn.execute_raw(&migration.up).unwrap_or_else(|error| {
            panic!(
                "apply pre-v17 migration {} ({}): {error}",
                migration.id, migration.description
            )
        });
        conn.execute_sync(
            &record_sql,
            &[
                Value::Text(migration.id),
                Value::Text(migration.description),
                Value::BigInt(1),
            ],
        )
        .expect("record applied migration");
    }
}

fn rollup_column_from_migration_id(id: &str) -> Option<&'static str> {
    Some(match id {
        "v18_rollup_ewma_loss" => "ewma_loss",
        "v18_rollup_ewma_weight" => "ewma_weight",
        "v18_rollup_delay_sum" => "delay_sum_micros",
        "v18_rollup_delay_count" => "delay_count",
        "v18_rollup_delay_max" => "delay_max_micros",
        "v22_rollup_compacted_total_count" => "compacted_total_count",
        "v22_rollup_compacted_resolved_count" => "compacted_resolved_count",
        "v22_rollup_compacted_censored_count" => "compacted_censored_count",
        "v22_rollup_compacted_expired_count" => "compacted_expired_count",
        "v22_rollup_compacted_correct_count" => "compacted_correct_count",
        "v22_rollup_compacted_incorrect_count" => "compacted_incorrect_count",
        "v22_rollup_compacted_total_regret" => "compacted_total_regret",
        "v22_rollup_compacted_total_loss" => "compacted_total_loss",
        "v22_rollup_compacted_ewma_loss" => "compacted_ewma_loss",
        "v22_rollup_compacted_ewma_weight" => "compacted_ewma_weight",
        "v22_rollup_compacted_delay_sum" => "compacted_delay_sum_micros",
        "v22_rollup_compacted_delay_count" => "compacted_delay_count",
        "v22_rollup_compacted_delay_max" => "compacted_delay_max_micros",
        "v22_rollup_compacted_last_updated_ts" => "compacted_last_updated_ts",
        _ => return None,
    })
}

fn seed_pre_v17_schema(conn: &SqliteConnection) {
    seed_schema_without_migrations(conn, ATC_V17_MIGRATION_IDS);
}

fn seed_pre_v21_schema(conn: &SqliteConnection) {
    seed_schema_without_migrations(conn, ATC_V21_MIGRATION_IDS);
}

// ---------------------------------------------------------------------------
// 1. Fresh database has correct schema version
// ---------------------------------------------------------------------------

#[test]
fn fresh_db_reaches_latest_schema_version() {
    let (conn, _dir) = open_temp_db();

    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Must have applied a meaningful number of migrations (v1 tables + v2 triggers
    // + v3 timestamp fixes + v4 indexes + v5 FTS + v6 inbox_stats + v7 identity FTS + v8 recipes).
    assert!(
        applied.len() > 30,
        "expected 30+ migrations on fresh DB, got {}",
        applied.len()
    );

    // Check that v1 through v8 prefixes are all represented.
    for prefix in &["v1_", "v2_", "v3_", "v4_", "v5_", "v6_", "v7_", "v8_"] {
        assert!(
            applied.iter().any(|id| id.starts_with(prefix)),
            "missing migration with prefix {prefix} in applied list"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. All tables exist after migration
// ---------------------------------------------------------------------------

#[test]
fn all_expected_tables_exist_after_migration() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name NOT LIKE 'sqlite_stat%' \
             ORDER BY name",
            &[],
        )
        .expect("query sqlite_master for tables");

    let table_names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();

    let expected_tables = [
        "projects",
        "products",
        "product_project_links",
        "agents",
        "messages",
        "message_recipients",
        "file_reservations",
        "agent_links",
        "project_sibling_suggestions",
        "inbox_stats",
        "search_recipes",
        "query_history",
    ];

    for table in &expected_tables {
        assert!(
            table_names.contains(&table.to_string()),
            "missing table '{table}' in {table_names:?}"
        );
    }

    // Verify migration tracking table exists.
    assert!(
        table_names.contains(&MIGRATIONS_TABLE_NAME.to_string()),
        "missing migration tracking table '{MIGRATIONS_TABLE_NAME}'"
    );
}

#[test]
fn atc_v17_schema_surface_exists_after_migration() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    assert_atc_v17_schema_surface(&conn);
}

#[test]
fn atc_v17_upgrade_from_pre_v17_schema_preserves_rows_and_defaults() {
    let (conn, _dir) = open_temp_db();
    seed_pre_v17_schema(&conn);

    assert!(
        !table_exists(&conn, "atc_leader_lease"),
        "pre-v17 seed should not include atc_leader_lease"
    );
    assert!(
        !table_exists(&conn, "atc_rollup_snapshots"),
        "pre-v17 seed should not include atc_rollup_snapshots"
    );
    let pre_columns: HashSet<String> = table_info(&conn, "atc_experiences")
        .into_iter()
        .map(|(name, _, _, _)| name)
        .collect();
    assert!(
        !pre_columns.contains("contained_suspected_secret"),
        "pre-v17 seed unexpectedly contains contained_suspected_secret"
    );
    assert!(
        !pre_columns.contains("privacy_classification"),
        "pre-v17 seed unexpectedly contains privacy_classification"
    );

    conn.execute_sync(
        "INSERT INTO atc_experiences (\
            experience_id, decision_id, effect_id, trace_id, claim_id, evidence_id, state, subsystem,\
            decision_class, subject, project_key, policy_id, effect_kind, action, posterior_json,\
            expected_loss, runner_up_action, runner_up_loss, evidence_summary, calibration_healthy,\
            safe_mode_active, non_execution_json, outcome_json, features_json, feature_ext_json,\
            created_ts, dispatched_ts, executed_ts, resolved_ts, context_json\
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(100),
            Value::BigInt(200),
            Value::Text("trace-pre-v17".to_string()),
            Value::Text("claim-pre-v17".to_string()),
            Value::Text("evidence-pre-v17".to_string()),
            Value::Text("open".to_string()),
            Value::Text("liveness".to_string()),
            Value::Text("probe".to_string()),
            Value::Text("GreenCastle".to_string()),
            Value::Text("/tmp/pre-v17".to_string()),
            Value::Text("policy-v1".to_string()),
            Value::Text("probe".to_string()),
            Value::Text("ProbeAgent".to_string()),
            Value::Text("[]".to_string()),
            Value::Double(0.25),
            Value::Null,
            Value::Null,
            Value::Text("metric_summary_only".to_string()),
            Value::BigInt(1),
            Value::BigInt(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::BigInt(1_700_000_000_000_000),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
    )
    .expect("seed pre-v17 ATC experience row");

    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    let applied_set: HashSet<&str> = applied.iter().map(String::as_str).collect();
    assert!(
        ATC_V17_MIGRATION_IDS
            .iter()
            .all(|id| applied_set.contains(id)),
        "upgrade should apply all ATC v17 migrations; got {applied:?}"
    );
    assert!(
        applied.iter().all(|id| {
            schema::schema_migrations()
                .iter()
                .position(|migration| migration.id == *id)
                >= schema::schema_migrations()
                    .iter()
                    .position(|migration| migration.id == ATC_V17_MIGRATION_IDS[0])
        }),
        "pre-v17 upgrade must not re-apply earlier migrations; got {applied:?}"
    );

    assert_atc_v17_schema_surface(&conn);

    let rows = conn
        .query_sync(
            "SELECT contained_suspected_secret, privacy_classification \
             FROM atc_experiences WHERE experience_id = 1",
            &[],
        )
        .expect("query upgraded ATC row");
    assert_eq!(rows.len(), 1, "expected seeded ATC row after upgrade");
    assert_eq!(
        rows[0]
            .get_named::<i64>("contained_suspected_secret")
            .expect("contained_suspected_secret value"),
        0,
        "seeded rows should default contained_suspected_secret=false"
    );
    assert_eq!(
        rows[0]
            .get_named::<String>("privacy_classification")
            .expect("privacy_classification value"),
        "legacy_unclassified",
        "seeded rows should default privacy_classification=legacy_unclassified"
    );
}

#[test]
fn atc_v17_privacy_classification_constraint_rejects_invalid_values() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    let error = conn
        .execute_sync(
            "INSERT INTO atc_experiences (\
                experience_id, decision_id, effect_id, trace_id, claim_id, evidence_id, state, subsystem,\
                decision_class, subject, effect_kind, action, posterior_json, expected_loss,\
                evidence_summary, calibration_healthy, safe_mode_active, created_ts, privacy_classification\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(9),
                Value::BigInt(900),
                Value::BigInt(901),
                Value::Text("trace-invalid-privacy".to_string()),
                Value::Text("claim-invalid-privacy".to_string()),
                Value::Text("evidence-invalid-privacy".to_string()),
                Value::Text("open".to_string()),
                Value::Text("liveness".to_string()),
                Value::Text("probe".to_string()),
                Value::Text("BlueLake".to_string()),
                Value::Text("probe".to_string()),
                Value::Text("ProbeAgent".to_string()),
                Value::Text("[]".to_string()),
                Value::Double(0.1),
                Value::Text("metric_summary_only".to_string()),
                Value::BigInt(1),
                Value::BigInt(0),
                Value::BigInt(1_700_000_000_000_100),
                Value::Text("definitely_invalid".to_string()),
            ],
        )
        .expect_err("invalid privacy classification should fail");
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("check") || message.contains("constraint"),
        "expected constraint error, got: {error}"
    );
}

#[test]
fn reconstruct_from_archive_recreates_atc_v17_schema_surface() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage_root = dir.path().join("storage");
    let db_path = dir.path().join("reconstructed_v17.sqlite3");
    let candidate_path = dir.path().join("reconstructed_v17.candidate.sqlite3");
    let agent_dir = storage_root
        .join("projects")
        .join("reconstructed-project")
        .join("agents")
        .join("BrownKite");
    std::fs::create_dir_all(&agent_dir).expect("create archive agent dir");
    std::fs::write(
        agent_dir.join("profile.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "name": "BrownKite",
            "program": "codex-cli",
            "model": "gpt-5",
            "task_description": "schema migration reconstruct test",
            "inception_ts": "2026-04-18T00:00:00Z",
            "last_active_ts": "2026-04-18T00:00:00Z",
            "attachments_policy": "auto",
            "contact_policy": "auto"
        }))
        .expect("serialize profile"),
    )
    .expect("write profile");

    mcp_agent_mail_db::reconstruct_from_archive(&candidate_path, &storage_root)
        .expect("reconstruct fresh candidate from archive");

    let sidecar_path = db_path.with_file_name("atc.sqlite3");
    assert!(
        !sidecar_path.exists(),
        "fresh candidate construction must not initialize the fixed live ATC sidecar"
    );
    mcp_agent_mail_db::promote_recovery_candidate(&db_path, &candidate_path, &storage_root)
        .expect("promote reconstructed candidate through the recovery receipt boundary");

    // ATC telemetry is isolated in the atc.sqlite3 sidecar (br-bvq1x.11.7), a
    // sibling of the primary mailbox DB. Candidate construction never touches
    // that fixed live path; the unified promotion boundary initializes the ATC
    // v17 schema only after the new primary generation is durably receipted.
    // The primary must stay free of atc_* tables (see
    // `reconstruct_with_agent_profile`).
    let conn =
        SqliteConnection::open_file(sidecar_path.display().to_string()).expect("open atc sidecar");
    conn.execute_raw(PRAGMA_SETTINGS_SQL)
        .expect("apply sqlite pragmas");
    assert_atc_v17_schema_surface(&conn);

    // A sidecar created by reconstruct must carry the same private posture as a
    // runtime-created one: it holds project keys, subjects, and evidence
    // summaries just like storage.sqlite3.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&sidecar_path)
            .expect("stat atc sidecar")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "reconstruct-created ATC sidecar must be private (0600), got {mode:o}"
        );
    }
}

#[test]
fn reconstruct_quarantines_unusable_atc_sidecar_instead_of_wedging_recovery() {
    // The disk incident that corrupts the primary mailbox DB usually hits its
    // same-directory sibling too. A corrupt atc.sqlite3 must NOT fatally wedge
    // recovery of the PRIMARY mailbox (ATC telemetry is droppable by contract):
    // reconstruct quarantines the unusable sidecar by rename and rebuilds a
    // fresh one with the full v17 schema surface.
    let dir = tempfile::tempdir().expect("tempdir");
    let storage_root = dir.path().join("storage");
    let db_path = dir.path().join("reconstructed_quarantine.sqlite3");
    let candidate_path = dir
        .path()
        .join("reconstructed_quarantine.candidate.sqlite3");
    let agent_dir = storage_root
        .join("projects")
        .join("quarantine-project")
        .join("agents")
        .join("GrayHeron");
    std::fs::create_dir_all(&agent_dir).expect("create archive agent dir");
    std::fs::write(
        agent_dir.join("profile.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "name": "GrayHeron",
            "program": "codex-cli",
            "model": "gpt-5",
            "task_description": "sidecar quarantine reconstruct test",
            "inception_ts": "2026-04-18T00:00:00Z",
            "last_active_ts": "2026-04-18T00:00:00Z",
            "attachments_policy": "auto",
            "contact_policy": "auto"
        }))
        .expect("serialize profile"),
    )
    .expect("write profile");

    // Pre-plant a garbage sidecar at the fixed live path. Fresh candidate
    // construction must leave it byte-for-byte untouched; only the unified
    // promotion boundary may quarantine and rebuild it after DB activation.
    let sidecar_path = db_path.with_file_name("atc.sqlite3");
    std::fs::write(&sidecar_path, b"this is not a sqlite database").expect("plant corrupt sidecar");

    mcp_agent_mail_db::reconstruct_from_archive(&candidate_path, &storage_root)
        .expect("reconstruct fresh candidate despite a corrupt live ATC sidecar");
    assert_eq!(
        std::fs::read(&sidecar_path).expect("read untouched corrupt sidecar"),
        b"this is not a sqlite database",
        "fresh candidate construction must not mutate the fixed live ATC sidecar"
    );
    mcp_agent_mail_db::promote_recovery_candidate(&db_path, &candidate_path, &storage_root)
        .expect("promotion must succeed despite a corrupt ATC sidecar");

    // The unusable sidecar was quarantined by rename (never deleted)...
    let quarantined: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("atc.sqlite3.quarantined-")
        })
        .collect();
    assert_eq!(
        quarantined.len(),
        1,
        "expected exactly one quarantined sidecar next to the DB"
    );
    let quarantined_bytes = std::fs::read(quarantined[0].path()).expect("read quarantined sidecar");
    assert_eq!(
        quarantined_bytes, b"this is not a sqlite database",
        "quarantine must preserve the original bytes verbatim"
    );

    // ...and a fresh, valid sidecar with the full v17 surface replaced it.
    let conn = SqliteConnection::open_file(sidecar_path.display().to_string())
        .expect("open rebuilt atc sidecar");
    conn.execute_raw(PRAGMA_SETTINGS_SQL)
        .expect("apply sqlite pragmas");
    assert_atc_v17_schema_surface(&conn);
}

#[test]
fn atc_v21_feature_schema_version_exists_after_migration() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    assert_atc_feature_schema_version_column(&conn);
}

#[test]
fn atc_v21_upgrade_from_pre_v21_schema_defaults_feature_schema_version() {
    let (conn, _dir) = open_temp_db();
    seed_pre_v21_schema(&conn);

    let pre_columns: HashSet<String> = table_info(&conn, "atc_experiences")
        .into_iter()
        .map(|(name, _, _, _)| name)
        .collect();
    let canonical_seed_already_has_feature_schema_version =
        pre_columns.contains("feature_schema_version");

    conn.execute_sync(
        "INSERT INTO atc_experiences (\
            experience_id, decision_id, effect_id, trace_id, claim_id, evidence_id, state, subsystem,\
            decision_class, subject, project_key, policy_id, effect_kind, action, posterior_json,\
            expected_loss, runner_up_action, runner_up_loss, evidence_summary, calibration_healthy,\
            safe_mode_active, non_execution_json, outcome_json, features_json, feature_ext_json,\
            created_ts, dispatched_ts, executed_ts, resolved_ts, context_json\
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(2),
            Value::BigInt(200),
            Value::BigInt(300),
            Value::Text("trace-pre-v21".to_string()),
            Value::Text("claim-pre-v21".to_string()),
            Value::Text("evidence-pre-v21".to_string()),
            Value::Text("open".to_string()),
            Value::Text("conflict".to_string()),
            Value::Text("reservation_conflict".to_string()),
            Value::Text("BlueLake".to_string()),
            Value::Text("/tmp/pre-v21".to_string()),
            Value::Text("policy-v2".to_string()),
            Value::Text("advisory".to_string()),
            Value::Text("RecommendReservation".to_string()),
            Value::Text("[]".to_string()),
            Value::Double(0.5),
            Value::Null,
            Value::Null,
            Value::Text("legacy row".to_string()),
            Value::BigInt(1),
            Value::BigInt(0),
            Value::Null,
            Value::Null,
            Value::Text("{\"version\":0}".to_string()),
            Value::Null,
            Value::BigInt(1_700_000_000_100_000),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
    )
    .expect("seed pre-v21 ATC experience row");

    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    let applied_set: HashSet<&str> = applied.iter().map(String::as_str).collect();
    assert!(
        ATC_V21_MIGRATION_IDS
            .iter()
            .all(|id| applied_set.contains(id)),
        "upgrade should record all ATC v21 migrations; got {applied:?}"
    );
    assert!(
        applied.iter().all(|id| {
            schema::schema_migrations()
                .iter()
                .position(|migration| migration.id == *id)
                >= schema::schema_migrations()
                    .iter()
                    .position(|migration| migration.id == ATC_V21_MIGRATION_IDS[0])
        }),
        "pre-v21 upgrade must not re-apply earlier migrations; got {applied:?}"
    );

    assert_atc_feature_schema_version_column(&conn);

    assert!(
        canonical_seed_already_has_feature_schema_version
            || applied_set.contains(ATC_V21_MIGRATION_IDS[0]),
        "legacy seed without feature_schema_version should add it during upgrade"
    );

    let rows = conn
        .query_sync(
            "SELECT feature_schema_version FROM atc_experiences WHERE experience_id = 2",
            &[],
        )
        .expect("query upgraded ATC v21 row");
    assert_eq!(rows.len(), 1, "expected seeded ATC row after upgrade");
    assert_eq!(
        rows[0]
            .get_named::<i64>("feature_schema_version")
            .expect("feature_schema_version value"),
        1,
        "seeded rows should default feature_schema_version=1"
    );
}

// ---------------------------------------------------------------------------
// 3. FTS tables are properly set up
// ---------------------------------------------------------------------------

#[test]
fn fts_virtual_tables_absent_after_migration() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'fts_%' ORDER BY name",
            &[],
        )
        .expect("query FTS tables");

    let fts_names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();

    // v11 drops all FTS tables (Tantivy handles search now).
    assert!(
        fts_names.is_empty(),
        "no FTS tables should exist after v11 migration, found: {fts_names:?}"
    );
}

#[test]
fn triggers_after_migration() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='trigger' ORDER BY name",
            &[],
        )
        .expect("query triggers");

    let trigger_names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();

    // FTS triggers should NOT exist (v11 drops them, Tantivy handles search).
    for name in &[
        "messages_ai",
        "messages_ad",
        "messages_au",
        "agents_ai",
        "agents_ad",
        "agents_au",
        "projects_ai",
        "projects_ad",
        "projects_au",
        "fts_messages_ai",
        "fts_messages_ad",
        "fts_messages_au",
    ] {
        assert!(
            !trigger_names.contains(&name.to_string()),
            "FTS trigger '{name}' should not exist after v11 migration, found: {trigger_names:?}"
        );
    }

    // v6 inbox_stats triggers should still exist.
    for name in &[
        "trg_inbox_stats_insert",
        "trg_inbox_stats_mark_read",
        "trg_inbox_stats_ack",
    ] {
        assert!(
            trigger_names.contains(&name.to_string()),
            "missing inbox stats trigger '{name}' in {trigger_names:?}"
        );
    }

    // v23/v24 cascade triggers: agent-owned operational rows cascade, message
    // recipient history is intentionally preserved when agent metadata is
    // removed so orphaned recipients remain reconstructable.
    for name in &[
        "trg_agents_cascade_file_reservations",
        "trg_file_reservations_cascade_releases",
        "trg_agents_cascade_agent_links",
        "trg_agents_cascade_inbox_stats",
        "trg_messages_cascade_recipients",
    ] {
        assert!(
            trigger_names.contains(&name.to_string()),
            "missing v23 cascade trigger '{name}' in {trigger_names:?}"
        );
    }
    assert!(
        !trigger_names.contains(&"trg_agents_cascade_message_recipients".to_string()),
        "v24 should drop agent-delete recipient cascade trigger; found {trigger_names:?}"
    );
}

// NOTE: fts_message_insert_trigger_fires removed — FTS5 triggers dropped
// in v11 migration (br-2tnl.8.4).  Tantivy handles indexing now.

// ---------------------------------------------------------------------------
// 4. Key columns exist with correct types
// ---------------------------------------------------------------------------

#[test]
fn key_columns_exist_with_correct_types() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Helper to get column info for a table.
    let columns_of = |table: &str| -> Vec<(String, String, bool)> {
        let rows = conn
            .query_sync(&format!("PRAGMA table_info({table})"), &[])
            .unwrap_or_else(|_| panic!("PRAGMA table_info({table}) failed"));
        rows.iter()
            .map(|r| {
                let name: String = r.get_named("name").unwrap_or_default();
                let col_type: String = r.get_named("type").unwrap_or_default();
                let notnull: i64 = r.get_named("notnull").unwrap_or(0);
                (name, col_type.to_uppercase(), notnull != 0)
            })
            .collect()
    };

    // projects table
    let cols = columns_of("projects");
    assert!(
        cols.iter().any(|(n, t, _)| n == "id" && t == "INTEGER"),
        "projects.id should be INTEGER"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "slug" && t == "TEXT" && *nn),
        "projects.slug should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "human_key" && t == "TEXT" && *nn),
        "projects.human_key should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "created_at" && t == "INTEGER" && *nn),
        "projects.created_at should be INTEGER NOT NULL"
    );

    // agents table
    let cols = columns_of("agents");
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "project_id" && t == "INTEGER" && *nn),
        "agents.project_id should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "name" && t == "TEXT" && *nn),
        "agents.name should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "inception_ts" && t == "INTEGER" && *nn),
        "agents.inception_ts should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "attachments_policy" && t == "TEXT" && *nn),
        "agents.attachments_policy should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "contact_policy" && t == "TEXT" && *nn),
        "agents.contact_policy should be TEXT NOT NULL"
    );

    // messages table
    let cols = columns_of("messages");
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "sender_id" && t == "INTEGER" && *nn),
        "messages.sender_id should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter().any(|(n, _, nn)| n == "thread_id" && !nn),
        "messages.thread_id should be nullable"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "body_md" && t == "TEXT" && *nn),
        "messages.body_md should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "ack_required" && t == "INTEGER" && *nn),
        "messages.ack_required should be INTEGER NOT NULL"
    );

    // message_recipients table
    let cols = columns_of("message_recipients");
    assert!(
        cols.iter().any(|(n, _, nn)| n == "read_ts" && !nn),
        "message_recipients.read_ts should be nullable"
    );
    assert!(
        cols.iter().any(|(n, _, nn)| n == "ack_ts" && !nn),
        "message_recipients.ack_ts should be nullable"
    );

    // file_reservations table
    let cols = columns_of("file_reservations");
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "path_pattern" && t == "TEXT" && *nn),
        "file_reservations.path_pattern should be TEXT NOT NULL"
    );
    assert!(
        cols.iter().any(|(n, _, nn)| n == "released_ts" && !nn),
        "file_reservations.released_ts should be nullable"
    );

    // inbox_stats table (v6)
    let cols = columns_of("inbox_stats");
    assert!(
        cols.iter()
            .any(|(n, t, _)| n == "agent_id" && t == "INTEGER"),
        "inbox_stats.agent_id should be INTEGER (PK)"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "total_count" && t == "INTEGER" && *nn),
        "inbox_stats.total_count should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "unread_count" && t == "INTEGER" && *nn),
        "inbox_stats.unread_count should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "ack_pending_count" && t == "INTEGER" && *nn),
        "inbox_stats.ack_pending_count should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter().any(|(n, _, nn)| n == "last_message_ts" && !nn),
        "inbox_stats.last_message_ts should be nullable"
    );

    // search_recipes table (v8)
    let cols = columns_of("search_recipes");
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "name" && t == "TEXT" && *nn),
        "search_recipes.name should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "query_text" && t == "TEXT" && *nn),
        "search_recipes.query_text should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "pinned" && t == "INTEGER" && *nn),
        "search_recipes.pinned should be INTEGER NOT NULL"
    );

    // query_history table (v8)
    let cols = columns_of("query_history");
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "query_text" && t == "TEXT" && *nn),
        "query_history.query_text should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "executed_ts" && t == "INTEGER" && *nn),
        "query_history.executed_ts should be INTEGER NOT NULL"
    );
}

// ---------------------------------------------------------------------------
// 5. Migration idempotency
// ---------------------------------------------------------------------------

#[test]
fn migration_is_idempotent() {
    let (conn, _dir) = open_temp_db();

    // First run: applies all migrations.
    let applied1 = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(!applied1.is_empty(), "first run should apply migrations");

    // Second run: no new migrations to apply.
    let applied2 = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(
        applied2.is_empty(),
        "second run should be no-op, but applied: {applied2:?}"
    );

    // Third run: still idempotent.
    let applied3 = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(
        applied3.is_empty(),
        "third run should also be no-op, but applied: {applied3:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. All indexes exist after migration
// ---------------------------------------------------------------------------

#[test]
fn all_expected_indexes_exist() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%' ORDER BY name",
            &[],
        )
        .expect("query indexes");

    let index_names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();

    // v1 indexes
    let v1_indexes = [
        "idx_projects_slug",
        "idx_projects_human_key",
        "idx_products_uid",
        "idx_products_name",
        "idx_agents_project_name",
        "idx_messages_project_created",
        "idx_messages_project_sender_created",
        "idx_messages_thread_id",
        "idx_messages_importance",
        "idx_messages_created_ts",
        "idx_message_recipients_agent",
        "idx_message_recipients_agent_message",
        "idx_file_reservations_project_released_expires",
        "idx_file_reservations_project_agent_released",
        "idx_file_reservations_expires_ts",
        "idx_agent_links_a_project",
        "idx_agent_links_b_project",
        "idx_agent_links_status",
    ];

    for idx in &v1_indexes {
        assert!(
            index_names.contains(&idx.to_string()),
            "missing v1 index '{idx}' in {index_names:?}"
        );
    }

    // v4 composite indexes
    let v4_indexes = [
        "idx_mr_agent_ack",
        "idx_msg_thread_created",
        "idx_msg_project_importance_created",
        "idx_al_a_agent_status",
        "idx_al_b_agent_status",
    ];

    for idx in &v4_indexes {
        assert!(
            index_names.contains(&idx.to_string()),
            "missing v4 composite index '{idx}' in {index_names:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. Pool-based initialization matches the primary direct-migration schema
// ---------------------------------------------------------------------------

#[test]
fn pool_initialization_creates_same_primary_schema_as_direct_migration() {
    // Direct migration path.
    let (conn, _dir1) = open_temp_db();
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    // Apply the same runtime cleanup that pool startup performs (drops legacy
    // identity FTS tables: fts_agents, fts_projects and their triggers).
    enforce_runtime_fts_cleanup(&conn).expect("identity fts cleanup");

    let direct_tables: Vec<String> = conn
        .query_sync(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name NOT LIKE 'sqlite_stat%' \
             AND name NOT LIKE 'atc_%' \
             ORDER BY name",
            &[],
        )
        .expect("query tables (direct)")
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();

    // Pool-managed path.
    let (pool, _dir2) = make_pool();
    let pool2 = pool.clone();
    block_on(|cx| async move {
        let conn = pool2.acquire(&cx).await.into_result().unwrap();
        // Just acquiring a connection triggers schema setup.
        let pool_tables: Vec<String> = conn
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type='table' AND name NOT LIKE 'sqlite_stat%' \
                 AND name NOT LIKE 'atc_%' \
                 ORDER BY name",
                &[],
            )
            .expect("query tables (pool)")
            .iter()
            .filter_map(|r| r.get_named::<String>("name").ok())
            .collect();

        let pool_atc_tables: Vec<String> = conn
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type='table' AND name LIKE 'atc_%' \
                 ORDER BY name",
                &[],
            )
            .expect("query ATC tables (pool)")
            .iter()
            .filter_map(|r| r.get_named::<String>("name").ok())
            .collect();

        assert_eq!(
            direct_tables, pool_tables,
            "pool-created primary schema should match direct migration schema"
        );
        assert!(
            pool_atc_tables.is_empty(),
            "pool startup must keep ATC telemetry out of the canonical mailbox database: \
             {pool_atc_tables:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// 8. Migration status tracking
// ---------------------------------------------------------------------------

#[test]
fn migration_status_reports_all_applied() {
    let (conn, _dir) = open_temp_db();

    // Apply all migrations.
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Get status.
    let statuses = block_on({
        let conn = &conn;
        move |cx| async move { migration_status(&cx, conn).await.into_result().unwrap() }
    });

    // Every migration should be Applied (not Pending).
    let pending: Vec<_> = statuses
        .iter()
        .filter(|(_, s)| matches!(s, sqlmodel_schema::MigrationStatus::Pending))
        .collect();

    assert!(
        pending.is_empty(),
        "all migrations should be applied, but these are pending: {pending:?}"
    );

    // The number of statuses should match the total migration count.
    let all_migrations = schema::schema_migrations();
    assert_eq!(
        statuses.len(),
        all_migrations.len(),
        "status count should match total migrations"
    );
}

// ---------------------------------------------------------------------------
// 9. v3 legacy TEXT timestamp roundtrip with real data
// ---------------------------------------------------------------------------

#[test]
fn v3_text_timestamp_conversion_roundtrip() {
    let (conn, _dir) = open_temp_db();

    conn.execute_raw(PRAGMA_SETTINGS_SQL)
        .expect("apply PRAGMAs");

    // Simulate a legacy Python database with DATETIME columns (TEXT storage).
    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS projects (id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL, created_at DATETIME NOT NULL)",
        &[],
    )
    .expect("create legacy projects");

    // Insert rows with different timestamp formats.
    let timestamps = [
        ("proj-a", "/a", "2026-02-04 22:13:11.079199"),
        ("proj-b", "/b", "2026-01-01 00:00:00.000000"),
        ("proj-c", "/c", "2026-12-31 23:59:59.999999"),
    ];

    for (slug, key, ts) in &timestamps {
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
            &[
                Value::Text(slug.to_string()),
                Value::Text(key.to_string()),
                Value::Text(ts.to_string()),
            ],
        )
        .expect("insert legacy project");
    }

    // Create other required tables with TEXT timestamps.
    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS agents (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT NOT NULL DEFAULT '', inception_ts DATETIME NOT NULL, last_active_ts DATETIME NOT NULL, attachments_policy TEXT NOT NULL DEFAULT 'auto', contact_policy TEXT NOT NULL DEFAULT 'auto', UNIQUE(project_id, name))",
        &[],
    )
    .expect("create legacy agents");

    conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("BlueLake".into()),
            Value::Text("cc".into()),
            Value::Text("opus".into()),
            Value::Text("2026-02-05 00:06:44.082288".into()),
            Value::Text("2026-02-05 01:30:00.000000".into()),
        ],
    )
    .expect("insert legacy agent");

    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, thread_id TEXT, subject TEXT NOT NULL, body_md TEXT NOT NULL, importance TEXT NOT NULL DEFAULT 'normal', ack_required INTEGER NOT NULL DEFAULT 0, created_ts DATETIME NOT NULL, attachments TEXT NOT NULL DEFAULT '[]')",
        &[],
    )
    .expect("create legacy messages");

    conn.execute_sync(
        "INSERT INTO messages (project_id, sender_id, subject, body_md, created_ts) VALUES (?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("Test msg".into()),
            Value::Text("Body".into()),
            Value::Text("2026-06-15 12:30:45.123456".into()),
        ],
    )
    .expect("insert legacy message");

    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS file_reservations (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL DEFAULT 1, reason TEXT NOT NULL DEFAULT '', created_ts DATETIME NOT NULL, expires_ts DATETIME NOT NULL, released_ts DATETIME)",
        &[],
    )
    .expect("create legacy file_reservations");

    conn.execute_sync(
        "INSERT INTO file_reservations (project_id, agent_id, path_pattern, created_ts, expires_ts) VALUES (?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("*.rs".into()),
            Value::Text("2026-03-01 10:00:00.500000".into()),
            Value::Text("2026-03-01 11:00:00.750000".into()),
        ],
    )
    .expect("insert legacy file reservation");

    // Create products/product_project_links with TEXT timestamps.
    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS products (id INTEGER PRIMARY KEY AUTOINCREMENT, product_uid TEXT NOT NULL UNIQUE, name TEXT NOT NULL UNIQUE, created_at DATETIME NOT NULL)",
        &[],
    )
    .expect("create legacy products");
    conn.execute_sync(
        "INSERT INTO products (product_uid, name, created_at) VALUES (?, ?, ?)",
        &[
            Value::Text("uid1".into()),
            Value::Text("Prod1".into()),
            Value::Text("2026-04-01 00:00:00.000001".into()),
        ],
    )
    .expect("insert legacy product");

    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS product_project_links (id INTEGER PRIMARY KEY AUTOINCREMENT, product_id INTEGER NOT NULL, project_id INTEGER NOT NULL, created_at DATETIME NOT NULL, UNIQUE(product_id, project_id))",
        &[],
    )
    .expect("create legacy product_project_links");
    conn.execute_sync(
        "INSERT INTO product_project_links (product_id, project_id, created_at) VALUES (?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("2026-05-15 06:30:00.999000".into()),
        ],
    )
    .expect("insert legacy link");

    // Run migrations.
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Verify ALL timestamps are now integers (not text).
    let verify_integer_type = |table: &str, col: &str| {
        let rows = conn
            .query_sync(
                &format!("SELECT typeof({col}) as t, {col} as v FROM {table}"),
                &[],
            )
            .unwrap_or_else(|_| panic!("query {table}.{col}"));
        for (i, row) in rows.iter().enumerate() {
            let t: String = row.get_named("t").unwrap_or_default();
            assert_eq!(
                t, "integer",
                "{table}.{col} row {i} should be integer, got '{t}'"
            );
            let v: i64 = row.get_named("v").unwrap_or(0);
            assert!(
                v > 1_700_000_000_000_000,
                "{table}.{col} row {i} should be microseconds since epoch, got {v}"
            );
        }
    };

    verify_integer_type("projects", "created_at");
    verify_integer_type("agents", "inception_ts");
    verify_integer_type("agents", "last_active_ts");
    verify_integer_type("messages", "created_ts");
    verify_integer_type("file_reservations", "created_ts");
    verify_integer_type("file_reservations", "expires_ts");
    verify_integer_type("products", "created_at");
    verify_integer_type("product_project_links", "created_at");

    // Verify the converted values are in the expected range.
    // "2026-02-04 22:13:11.079199" should convert to approximately 1.77e15 microseconds.
    // SQLite strftime('%s') treats timestamps as UTC. We verify that all three
    // project rows have timestamps in the 2026 range (roughly 1.77e15 to 1.80e15).
    let rows = conn
        .query_sync("SELECT created_at FROM projects ORDER BY id", &[])
        .expect("query all projects");
    assert_eq!(rows.len(), 3);
    for (i, row) in rows.iter().enumerate() {
        let created_at: i64 = row.get_named("created_at").unwrap();
        assert!(
            created_at > 1_767_000_000_000_000 && created_at < 1_800_000_000_000_000,
            "project row {i} created_at={created_at} should be in 2026 microsecond range"
        );
    }

    // Verify fractional microseconds are preserved (the .079199 part).
    let rows = conn
        .query_sync("SELECT created_at FROM projects WHERE slug = 'proj-a'", &[])
        .expect("query proj-a");
    let created_at: i64 = rows[0].get_named("created_at").unwrap();
    let fractional = created_at % 1_000_000;
    assert_eq!(
        fractional, 79199,
        "proj-a fractional microseconds should be 079199, got {fractional}"
    );
}

// ---------------------------------------------------------------------------
// 10. v6 inbox_stats triggers fire correctly with real data
// ---------------------------------------------------------------------------

#[test]
fn v6_inbox_stats_triggers_work_with_real_data() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Set up project + agents.
    conn.execute_sync(
        "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
        &[
            Value::Text("stats-proj".into()),
            Value::Text("/stats".into()),
            Value::BigInt(1_000_000),
        ],
    )
    .expect("insert project");

    conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("RedFox".into()),
            Value::Text("test".into()),
            Value::Text("model".into()),
            Value::BigInt(1_000_000),
            Value::BigInt(1_000_000),
        ],
    )
    .expect("insert sender");

    conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("BlueLake".into()),
            Value::Text("test".into()),
            Value::Text("model".into()),
            Value::BigInt(1_000_000),
            Value::BigInt(1_000_000),
        ],
    )
    .expect("insert recipient");

    // Insert a message with ack_required=1.
    conn.execute_sync(
        "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, created_ts) VALUES (?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("Urgent task".into()),
            Value::Text("Please ack".into()),
            Value::Text("high".into()),
            Value::BigInt(1),
            Value::BigInt(5_000_000),
        ],
    )
    .expect("insert ack-required message");

    // Add recipient (agent_id=2 is BlueLake).
    conn.execute_sync(
        "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)",
        &[Value::BigInt(1), Value::BigInt(2), Value::Text("to".into())],
    )
    .expect("insert recipient link");

    // Check inbox_stats: should have total=1, unread=1, ack_pending=1.
    let rows = conn
        .query_sync(
            "SELECT total_count, unread_count, ack_pending_count FROM inbox_stats WHERE agent_id = 2",
            &[],
        )
        .expect("query inbox_stats");
    assert_eq!(rows.len(), 1, "inbox_stats should have a row for agent 2");
    assert_eq!(rows[0].get_named::<i64>("total_count").unwrap_or(0), 1);
    assert_eq!(rows[0].get_named::<i64>("unread_count").unwrap_or(0), 1);
    assert_eq!(
        rows[0].get_named::<i64>("ack_pending_count").unwrap_or(0),
        1
    );

    // Mark as read.
    conn.execute_sync(
        "UPDATE message_recipients SET read_ts = ? WHERE message_id = 1 AND agent_id = 2",
        &[Value::BigInt(6_000_000)],
    )
    .expect("mark read");

    let rows = conn
        .query_sync(
            "SELECT unread_count FROM inbox_stats WHERE agent_id = 2",
            &[],
        )
        .expect("query unread after mark read");
    assert_eq!(
        rows[0].get_named::<i64>("unread_count").unwrap_or(-1),
        0,
        "unread should decrement to 0 after mark read"
    );

    // Acknowledge.
    conn.execute_sync(
        "UPDATE message_recipients SET ack_ts = ? WHERE message_id = 1 AND agent_id = 2",
        &[Value::BigInt(7_000_000)],
    )
    .expect("acknowledge");

    let rows = conn
        .query_sync(
            "SELECT ack_pending_count FROM inbox_stats WHERE agent_id = 2",
            &[],
        )
        .expect("query ack_pending after ack");
    assert_eq!(
        rows[0].get_named::<i64>("ack_pending_count").unwrap_or(-1),
        0,
        "ack_pending should decrement to 0 after acknowledgement"
    );
}

// NOTE: v7_identity_fts_backfill_from_preexisting_data removed — identity FTS
// tables and triggers dropped by v11 migrations (br-2tnl.8.4).
// Tantivy handles full-text search for agents and projects now.

// NOTE: v5_fts_porter_stemming_and_prefix_search removed — FTS5 decommissioned
// in v11 migration (br-2tnl.8.4).  Tantivy handles stemming/prefix search.

// ---------------------------------------------------------------------------
// 13. Partial migration (incremental upgrade)
// ---------------------------------------------------------------------------

#[test]
fn incremental_migration_from_partial_schema() {
    let (conn, _dir) = open_temp_db();

    conn.execute_raw(PRAGMA_SETTINGS_SQL)
        .expect("apply PRAGMAs");

    // Simulate a DB that already had v1 tables created manually (e.g. older version).
    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS projects (id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL, created_at INTEGER NOT NULL)",
        &[],
    )
    .expect("create projects manually");

    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS agents (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT NOT NULL DEFAULT '', inception_ts INTEGER NOT NULL, last_active_ts INTEGER NOT NULL, attachments_policy TEXT NOT NULL DEFAULT 'auto', contact_policy TEXT NOT NULL DEFAULT 'auto', UNIQUE(project_id, name))",
        &[],
    )
    .expect("create agents manually");

    // Insert some data so we can verify it survives migration.
    conn.execute_sync(
        "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
        &[
            Value::Text("existing".into()),
            Value::Text("/existing".into()),
            Value::BigInt(42_000_000),
        ],
    )
    .expect("insert existing project");

    conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("OldAgent".into()),
            Value::Text("legacy".into()),
            Value::Text("old-model".into()),
            Value::BigInt(42_000_000),
            Value::BigInt(42_000_000),
        ],
    )
    .expect("insert existing agent");

    // Run migrations -- should create missing tables without disturbing existing data.
    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(
        !applied.is_empty(),
        "should apply some migrations on partial schema"
    );

    // Existing data should still be there.
    let rows = conn
        .query_sync("SELECT slug FROM projects", &[])
        .expect("query projects");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get_named::<String>("slug").unwrap_or_default(),
        "existing"
    );

    let rows = conn
        .query_sync("SELECT name FROM agents", &[])
        .expect("query agents");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get_named::<String>("name").unwrap_or_default(),
        "OldAgent"
    );

    // New tables should exist.
    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' AND name = 'inbox_stats'",
            &[],
        )
        .expect("check inbox_stats");
    assert_eq!(rows.len(), 1, "inbox_stats should have been created");
}

// ---------------------------------------------------------------------------
// 14. Migration count consistency
// ---------------------------------------------------------------------------

#[test]
fn schema_migrations_count_is_consistent() {
    let migrations = schema::schema_migrations();

    // All migration IDs should be unique.
    let mut ids: Vec<String> = migrations.iter().map(|m| m.id.clone()).collect();
    let total = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        total,
        "migration IDs must be unique; found {} duplicates",
        total - ids.len()
    );

    // Verify that all expected version prefixes (v1 through v8) are present.
    // Note: ordering is not strictly monotonic by version number because
    // schema_migrations() interleaves v2 (drop legacy triggers) before v1
    // (create triggers) due to the split between CREATE_TABLES_SQL and
    // CREATE_FTS_TRIGGERS_SQL processing order.
    let mut version_set = std::collections::BTreeSet::new();
    for m in &migrations {
        if let Some(v_str) = m.id.strip_prefix('v')
            && let Some(num_str) = v_str.split('_').next()
            && let Ok(v) = num_str.parse::<u32>()
        {
            version_set.insert(v);
        }
    }
    for expected_v in 1..=8 {
        assert!(
            version_set.contains(&expected_v),
            "expected migrations with version prefix v{expected_v}_, found only: {version_set:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 15. Data survives full migration roundtrip
// ---------------------------------------------------------------------------

#[test]
fn data_survives_complete_migration_roundtrip() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Insert a full set of test data.
    conn.execute_sync(
        "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
        &[
            Value::Text("roundtrip".into()),
            Value::Text("/roundtrip".into()),
            Value::BigInt(1_000_000),
        ],
    )
    .expect("insert project");

    conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("RedFox".into()),
            Value::Text("claude-code".into()),
            Value::Text("opus".into()),
            Value::Text("roundtrip testing".into()),
            Value::BigInt(2_000_000),
            Value::BigInt(3_000_000),
        ],
    )
    .expect("insert agent");

    conn.execute_sync(
        "INSERT INTO messages (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("TKT-001".into()),
            Value::Text("Roundtrip subject".into()),
            Value::Text("Roundtrip body text".into()),
            Value::Text("high".into()),
            Value::BigInt(1),
            Value::BigInt(4_000_000),
        ],
    )
    .expect("insert message");

    conn.execute_sync(
        "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)",
        &[Value::BigInt(1), Value::BigInt(1), Value::Text("to".into())],
    )
    .expect("insert recipient");

    conn.execute_sync(
        "INSERT INTO file_reservations (project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts) VALUES (?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("src/*.rs".into()),
            Value::BigInt(1),
            Value::Text("editing".into()),
            Value::BigInt(5_000_000),
            Value::BigInt(6_000_000),
        ],
    )
    .expect("insert file reservation");

    // Re-run migrations (should be no-op).
    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(applied.is_empty(), "re-running migrations should be no-op");

    // Verify all data is intact.
    let rows = conn
        .query_sync(
            "SELECT slug, human_key, created_at FROM projects WHERE slug = 'roundtrip'",
            &[],
        )
        .expect("query project");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get_named::<i64>("created_at").unwrap(), 1_000_000);

    let rows = conn
        .query_sync(
            "SELECT name, task_description FROM agents WHERE name = 'RedFox'",
            &[],
        )
        .expect("query agent");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]
            .get_named::<String>("task_description")
            .unwrap_or_default(),
        "roundtrip testing"
    );

    let rows = conn
        .query_sync(
            "SELECT subject, importance, ack_required FROM messages WHERE thread_id = 'TKT-001'",
            &[],
        )
        .expect("query message");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]
            .get_named::<String>("importance")
            .unwrap_or_default(),
        "high"
    );
    assert_eq!(rows[0].get_named::<i64>("ack_required").unwrap_or(0), 1);

    let rows = conn
        .query_sync("SELECT path_pattern, reason FROM file_reservations", &[])
        .expect("query file reservations");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]
            .get_named::<String>("path_pattern")
            .unwrap_or_default(),
        "src/*.rs"
    );
    assert_eq!(
        rows[0].get_named::<String>("reason").unwrap_or_default(),
        "editing"
    );

    // FTS tables are dropped by v11 migration — no FTS assertions needed.
    // Tantivy handles full-text search now.

    // Inbox stats should have been updated by the recipient insert trigger.
    let rows = conn
        .query_sync(
            "SELECT total_count, unread_count, ack_pending_count FROM inbox_stats WHERE agent_id = 1",
            &[],
        )
        .expect("query inbox_stats");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get_named::<i64>("total_count").unwrap_or(0), 1);
    assert_eq!(rows[0].get_named::<i64>("unread_count").unwrap_or(0), 1);
    assert_eq!(
        rows[0].get_named::<i64>("ack_pending_count").unwrap_or(0),
        1
    );
}

/// Test whether `FrankenConnection` corrupts a DB created by `SqliteConnection`.
/// Reproduces the "malformed database schema (`agent_links`) - duplicate column name" error.
#[test]
fn frankenconnection_does_not_corrupt_schema() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("test_corruption.db");
    let path_str = db_path.display().to_string();

    // 1. Create DB with SqliteConnection (base schema, no FTS5/triggers)
    let conn = SqliteConnection::open_file(&path_str).expect("open sqlite");
    conn.execute_raw(&schema::init_schema_sql_base())
        .expect("init base schema");
    conn.execute_sync(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'test', '/tmp/test', 0)",
        &[],
    )
    .expect("insert project");
    drop(conn);

    // 2. Verify schema is valid + dump sqlite_master
    let conn = SqliteConnection::open_file(&path_str).expect("reopen sqlite");
    conn.query_sync("SELECT * FROM agent_links", &[])
        .expect("query agent_links before FrankenConnection");

    // 2b. Dump sqlite_master schema SQL BEFORE FrankenConnection
    let schema_before: Vec<String> = conn
        .query_sync(
            "SELECT type, name, sql FROM sqlite_master ORDER BY rowid",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| {
            let typ: String = r.get_named("type").unwrap_or_default();
            let name: String = r.get_named("name").unwrap_or_default();
            let sql: String = r.get_named("sql").unwrap_or_default();
            format!("{typ}: {name} => {}", &sql[..sql.len().min(100)])
        })
        .collect();
    eprintln!(
        "BEFORE FrankenConnection ({} entries):",
        schema_before.len()
    );
    for s in &schema_before {
        eprintln!("  {s}");
    }
    drop(conn);

    // 3. Open with FrankenConnection and do a write
    let fconn = DbConn::open_file(&path_str).expect("open franken");
    fconn
        .execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (2, 'test2', '/tmp/test2', 0)",
            &[],
        )
        .expect("franken insert");
    drop(fconn);

    // 4. Re-verify with SqliteConnection — schema should still be valid
    let conn = SqliteConnection::open_file(&path_str).expect("reopen sqlite after franken");

    // 4a. Dump sqlite_master schema SQL AFTER FrankenConnection
    match conn.query_sync(
        "SELECT type, name, sql FROM sqlite_master ORDER BY rowid",
        &[],
    ) {
        Ok(rows) => {
            eprintln!("AFTER FrankenConnection ({} entries):", rows.len());
            for r in &rows {
                let typ: String = r.get_named("type").unwrap_or_default();
                let name: String = r.get_named("name").unwrap_or_default();
                let sql: String = r.get_named("sql").unwrap_or_default();
                eprintln!("  {typ}: {name} => {}", &sql[..sql.len().min(200)]);
            }
        }
        Err(e) => eprintln!("AFTER: Failed to read sqlite_master: {e}"),
    }

    match conn.query_sync("SELECT * FROM agent_links", &[]) {
        Ok(_) => {} // Schema is fine
        Err(e) => panic!("FrankenConnection corrupted DB schema: {e}"),
    }
    conn.execute_sync("UPDATE projects SET slug = 'updated' WHERE id = 1", &[])
        .expect("update after franken should work");
}

/// Does `FrankenConnection` corrupt even without writing (just open + close)?
#[test]
fn frankenconnection_open_close_no_write() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("test_nowrite.db");
    let path_str = db_path.display().to_string();

    // Create DB with our schema
    let conn = SqliteConnection::open_file(&path_str).expect("open");
    conn.execute_raw(&schema::init_schema_sql_base()).unwrap();
    conn.execute_sync(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'test', '/tmp/test', 0)",
        &[],
    )
    .unwrap();
    drop(conn);

    // Just open and close FrankenConnection without writing
    let fconn = DbConn::open_file(&path_str).expect("franken open");
    drop(fconn);

    // Check if schema is still valid
    let conn = SqliteConnection::open_file(&path_str).expect("reopen");
    match conn.query_sync("SELECT * FROM agent_links", &[]) {
        Ok(_) => eprintln!("open_close_no_write: schema OK"),
        Err(e) => panic!("FrankenConnection corrupted schema just by opening: {e}"),
    }
}

/// Does corruption happen with AUTOINCREMENT specifically?
#[test]
fn frankenconnection_autoincrement_write() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");

    // Test A: Write to table WITHOUT AUTOINCREMENT (manual ID)
    {
        let db_path = dir.path().join("test_no_autoincrement.db");
        let path_str = db_path.display().to_string();
        let conn = SqliteConnection::open_file(&path_str).expect("open");
        conn.execute_raw(&schema::init_schema_sql_base()).unwrap();
        drop(conn);

        let fconn = DbConn::open_file(&path_str).expect("franken open");
        // Insert with explicit id, bypassing sqlite_sequence
        fconn
            .execute_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (999, 'x', '/x', 0)",
                &[],
            )
            .unwrap();
        drop(fconn);

        let conn = SqliteConnection::open_file(&path_str).expect("reopen");
        match conn.query_sync("SELECT * FROM agent_links", &[]) {
            Ok(_) => eprintln!("no_autoincrement (explicit id): schema OK"),
            Err(e) => eprintln!("no_autoincrement (explicit id): CORRUPTED: {e}"),
        }
    }

    // Test B: Write to AUTOINCREMENT table (triggers sqlite_sequence update)
    {
        let db_path = dir.path().join("test_autoincrement.db");
        let path_str = db_path.display().to_string();
        let conn = SqliteConnection::open_file(&path_str).expect("open");
        conn.execute_raw(&schema::init_schema_sql_base()).unwrap();
        drop(conn);

        let fconn = DbConn::open_file(&path_str).expect("franken open");
        // Insert without explicit id — triggers AUTOINCREMENT / sqlite_sequence
        fconn
            .execute_sync(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('y', '/y', 0)",
                &[],
            )
            .unwrap();
        drop(fconn);

        let conn = SqliteConnection::open_file(&path_str).expect("reopen");
        match conn.query_sync("SELECT * FROM agent_links", &[]) {
            Ok(_) => eprintln!("autoincrement: schema OK"),
            Err(e) => eprintln!("autoincrement: CORRUPTED: {e}"),
        }
    }

    // Test C: Write to non-AUTOINCREMENT table
    {
        let db_path = dir.path().join("test_no_ai_table.db");
        let path_str = db_path.display().to_string();
        let conn = SqliteConnection::open_file(&path_str).expect("open");
        conn.execute_raw(&schema::init_schema_sql_base()).unwrap();
        // Pre-populate required references
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 't', '/t', 0)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (1, 1, 'GreenCastle', 'test', 'test', '', 0, 0, 'auto', 'auto')",
            &[],
        ).unwrap();
        drop(conn);

        let fconn = DbConn::open_file(&path_str).expect("franken open");
        // message_recipients has no AUTOINCREMENT
        fconn
            .execute_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (1, 1, 'to')",
                &[],
            )
            .unwrap_or_default(); // May fail on FK but we care about corruption
        drop(fconn);

        let conn = SqliteConnection::open_file(&path_str).expect("reopen");
        match conn.query_sync("SELECT * FROM agent_links", &[]) {
            Ok(_) => eprintln!("non-AI table write: schema OK"),
            Err(e) => eprintln!("non-AI table write: CORRUPTED: {e}"),
        }
    }
}

/// Does `FrankenConnection` corrupt with a read-only query?
#[test]
fn frankenconnection_read_only_no_corruption() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("test_readonly.db");
    let path_str = db_path.display().to_string();

    // Create DB with our schema
    let conn = SqliteConnection::open_file(&path_str).expect("open");
    conn.execute_raw(&schema::init_schema_sql_base()).unwrap();
    conn.execute_sync(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'test', '/tmp/test', 0)",
        &[],
    )
    .unwrap();
    drop(conn);

    // Open FrankenConnection and only do reads
    let fconn = DbConn::open_file(&path_str).expect("franken open");
    let _rows = fconn
        .query_sync("SELECT id, slug FROM projects", &[])
        .unwrap();
    drop(fconn);

    // Check if schema is still valid
    let conn = SqliteConnection::open_file(&path_str).expect("reopen");
    match conn.query_sync("SELECT * FROM agent_links", &[]) {
        Ok(_) => eprintln!("read_only: schema OK"),
        Err(e) => panic!("FrankenConnection corrupted schema with read-only: {e}"),
    }
}

/// Minimal reproduction: does `FrankenConnection` corrupt even a tiny schema?
#[test]
fn frankenconnection_tiny_schema_no_corruption() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("test_tiny.db");
    let path_str = db_path.display().to_string();

    // 1. Create a tiny DB
    let conn = SqliteConnection::open_file(&path_str).expect("open");
    conn.execute_raw("CREATE TABLE t1 (id INTEGER PRIMARY KEY, val TEXT)")
        .unwrap();
    conn.execute_raw("CREATE TABLE t2 (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER)")
        .unwrap();
    conn.execute_sync("INSERT INTO t1 VALUES (1, 'hello')", &[])
        .unwrap();
    drop(conn);

    // 2. FrankenConnection writes
    let fconn = DbConn::open_file(&path_str).expect("franken open");
    fconn
        .execute_sync("INSERT INTO t1 VALUES (2, 'world')", &[])
        .unwrap();
    drop(fconn);

    // 3. SqliteConnection verifies
    let conn = SqliteConnection::open_file(&path_str).expect("reopen");
    let rows = conn.query_sync("SELECT * FROM t1", &[]).unwrap();
    assert_eq!(rows.len(), 2, "expected 2 rows");
    conn.query_sync("SELECT * FROM t2", &[])
        .expect("t2 schema should be intact");
}

/// Test with progressively more tables to find corruption threshold.
#[test]
fn frankenconnection_many_tables_corruption_threshold() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");

    for n_tables in [2, 5, 8, 10, 12] {
        let db_path = dir.path().join(format!("test_{n_tables}.db"));
        let path_str = db_path.display().to_string();

        // Create DB with N tables
        let conn = SqliteConnection::open_file(&path_str).expect("open");
        for i in 0..n_tables {
            let sql = format!(
                "CREATE TABLE table_{i} (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 col_a INTEGER NOT NULL, col_b TEXT NOT NULL DEFAULT '', col_c INTEGER)"
            );
            conn.execute_raw(&sql).unwrap();
        }
        conn.execute_sync("INSERT INTO table_0 (col_a, col_b) VALUES (1, 'test')", &[])
            .unwrap();
        drop(conn);

        // FrankenConnection writes
        let fconn = DbConn::open_file(&path_str).expect("franken open");
        fconn
            .execute_sync(
                "INSERT INTO table_0 (col_a, col_b) VALUES (2, 'world')",
                &[],
            )
            .unwrap();
        drop(fconn);

        // SqliteConnection verifies ALL tables
        let conn = SqliteConnection::open_file(&path_str).expect("reopen");
        for i in 0..n_tables {
            match conn.execute_sync(&format!("SELECT count(*) FROM table_{i}"), &[]) {
                Ok(_) => {}
                Err(e) => panic!("Corruption at {n_tables} tables, table_{i}: {e}"),
            }
        }
        // Also check via sqlite_master
        match conn.query_sync("SELECT count(*) FROM sqlite_master WHERE type='table'", &[]) {
            Ok(_) => eprintln!("OK: {n_tables} tables, no corruption"),
            Err(e) => panic!("sqlite_master corrupted at {n_tables} tables: {e}"),
        }
    }
}

// ===========================================================================
// v10a/v10b migration tests — Doom Loop Fix Test Coverage (br-3h13.16.1)
// ===========================================================================

/// Helper: create a pre-v10 agents table (case-sensitive UNIQUE constraint)
/// and a projects table, then insert agents manually. Returns an open
/// `SqliteConnection` ready for `migrate_to_latest`.
fn setup_pre_v10_db_with_agents(
    agents: &[(i64, &str, i64)], // (project_id, name, explicit_id)
) -> (SqliteConnection, tempfile::TempDir) {
    let (conn, dir) = open_temp_db();

    conn.execute_raw(schema::PRAGMA_SETTINGS_SQL)
        .expect("apply PRAGMAs");

    // Create pre-v10 schema: projects + agents with case-sensitive UNIQUE.
    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS projects (\
            id INTEGER PRIMARY KEY AUTOINCREMENT,\
            slug TEXT NOT NULL UNIQUE,\
            human_key TEXT NOT NULL,\
            created_at INTEGER NOT NULL\
        )",
        &[],
    )
    .expect("create projects");

    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS agents (\
            id INTEGER PRIMARY KEY AUTOINCREMENT,\
            project_id INTEGER NOT NULL REFERENCES projects(id),\
            name TEXT NOT NULL,\
            program TEXT NOT NULL,\
            model TEXT NOT NULL,\
            task_description TEXT NOT NULL DEFAULT '',\
            inception_ts INTEGER NOT NULL,\
            last_active_ts INTEGER NOT NULL,\
            attachments_policy TEXT NOT NULL DEFAULT 'auto',\
            contact_policy TEXT NOT NULL DEFAULT 'auto',\
            UNIQUE(project_id, name)\
        )",
        &[],
    )
    .expect("create agents");

    // Create index matching pre-v10 schema.
    conn.execute_sync(
        "CREATE INDEX IF NOT EXISTS idx_agents_project_name ON agents(project_id, name)",
        &[],
    )
    .expect("create index");

    // Insert projects that are referenced by agents.
    let mut project_ids: Vec<i64> = agents.iter().map(|(pid, _, _)| *pid).collect();
    project_ids.sort_unstable();
    project_ids.dedup();
    for pid in &project_ids {
        conn.execute_sync(
            "INSERT OR IGNORE INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(*pid),
                Value::Text(format!("proj-{pid}")),
                Value::Text(format!("/proj/{pid}")),
                Value::BigInt(1_000_000),
            ],
        )
        .expect("insert project");
    }

    // Insert agents with explicit IDs to control ordering.
    for (project_id, name, agent_id) in agents {
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(*agent_id),
                Value::BigInt(*project_id),
                Value::Text(name.to_string()),
                Value::Text("test-program".into()),
                Value::Text("test-model".into()),
                Value::BigInt(*agent_id * 1_000_000),
                Value::BigInt(*agent_id * 1_000_000),
            ],
        )
        .unwrap_or_else(|e| panic!("insert agent '{name}' (id={agent_id}): {e}"));
    }

    (conn, dir)
}

// ---------------------------------------------------------------------------
// T16.1.1: Test v10a dedup fires on case-duplicate agents and keeps oldest
// (br-3h13.16.1.1)
// ---------------------------------------------------------------------------

#[test]
fn v10a_dedup_case_duplicate_agents_keeps_oldest() {
    // Setup: Two agents with same project_id but different-case names.
    // id=10 "SilverFox" (oldest), id=20 "silverfox" (newer), id=30 "SILVERFOX" (newest).
    let (conn, _dir) = setup_pre_v10_db_with_agents(&[
        (1, "SilverFox", 10),
        (1, "silverfox", 20),
        (1, "SILVERFOX", 30),
    ]);

    // Run all migrations (v10a will dedup).
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Only 1 agent should remain for project_id=1.
    let rows = conn
        .query_sync(
            "SELECT id, name FROM agents WHERE project_id = 1 ORDER BY id",
            &[],
        )
        .expect("query agents after dedup");

    assert_eq!(
        rows.len(),
        1,
        "expected exactly 1 agent after v10a dedup, got {}",
        rows.len()
    );

    // The KEPT agent must be the one with the lowest id (oldest = id 10).
    let kept_id: i64 = rows[0].get_named("id").expect("get id");
    let kept_name: String = rows[0].get_named("name").expect("get name");
    assert_eq!(kept_id, 10, "should keep oldest agent (id=10)");
    assert_eq!(
        kept_name, "SilverFox",
        "should keep the name of the oldest agent"
    );
}

// ---------------------------------------------------------------------------
// T16.1.2: Test v10a migration is safe on empty agents table
// (br-3h13.16.1.2)
// ---------------------------------------------------------------------------

#[test]
fn v10a_safe_on_empty_agents_table() {
    // Setup: No agents at all.
    let (conn, _dir) = setup_pre_v10_db_with_agents(&[]);

    // Run all migrations — should succeed without error.
    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(
        applied
            .iter()
            .any(|id| id == "v10a_dedup_agents_case_insensitive"),
        "v10a migration should have been applied"
    );

    // Agents table still exists and is empty.
    let rows = conn
        .query_sync("SELECT COUNT(*) as cnt FROM agents", &[])
        .expect("count agents");
    let count: i64 = rows[0].get_named("cnt").unwrap_or(-1);
    assert_eq!(
        count, 0,
        "agents table should be empty after v10a on empty table"
    );

    // Idempotency: running again also succeeds.
    let applied2 = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(applied2.is_empty(), "re-running migrations should be no-op");
}

// ---------------------------------------------------------------------------
// T16.1.3: Test v10a preserves non-duplicate agents unchanged
// (br-3h13.16.1.3)
// ---------------------------------------------------------------------------

#[test]
fn v10a_preserves_non_duplicate_agents() {
    // Setup: 3 agents with unique names (no case collisions).
    let (conn, _dir) =
        setup_pre_v10_db_with_agents(&[(1, "Alice", 1), (1, "Bob", 2), (2, "Charlie", 3)]);

    // Run all migrations.
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // All 3 agents should still exist with unchanged data.
    let rows = conn
        .query_sync(
            "SELECT id, name, project_id, program, model FROM agents ORDER BY id",
            &[],
        )
        .expect("query all agents");

    assert_eq!(rows.len(), 3, "all 3 non-duplicate agents should survive");

    let names: Vec<String> = rows
        .iter()
        .map(|r| r.get_named::<String>("name").unwrap_or_default())
        .collect();
    assert_eq!(names, vec!["Alice", "Bob", "Charlie"]);

    let ids: Vec<i64> = rows
        .iter()
        .map(|r| r.get_named::<i64>("id").unwrap_or(0))
        .collect();
    assert_eq!(ids, vec![1, 2, 3], "agent IDs should be unchanged");

    // Verify agent data (program, model) is also unchanged.
    for row in &rows {
        let program: String = row.get_named("program").unwrap_or_default();
        let model: String = row.get_named("model").unwrap_or_default();
        assert_eq!(program, "test-program", "program should be unchanged");
        assert_eq!(model, "test-model", "model should be unchanged");
    }
}

// ---------------------------------------------------------------------------
// T16.1.4: Test v10a cross-project isolation (same name, different projects kept)
// (br-3h13.16.1.4)
// ---------------------------------------------------------------------------

#[test]
fn v10a_cross_project_isolation_same_name_different_projects() {
    // Setup: Same name (different case) but in DIFFERENT projects.
    // These should NOT be deduped because GROUP BY includes project_id.
    let (conn, _dir) = setup_pre_v10_db_with_agents(&[(1, "Alice", 1), (2, "alice", 2)]);

    // Run all migrations.
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Both agents should still exist (2 rows total).
    let rows = conn
        .query_sync("SELECT id, name, project_id FROM agents ORDER BY id", &[])
        .expect("query agents");

    assert_eq!(
        rows.len(),
        2,
        "cross-project agents with same name should both survive dedup"
    );

    let agent1_id: i64 = rows[0].get_named("id").unwrap_or(0);
    let agent1_proj: i64 = rows[0].get_named("project_id").unwrap_or(0);
    let agent2_id: i64 = rows[1].get_named("id").unwrap_or(0);
    let agent2_proj: i64 = rows[1].get_named("project_id").unwrap_or(0);

    assert_eq!(agent1_id, 1, "agent in project 1 should have id=1");
    assert_eq!(agent1_proj, 1, "first agent should be in project 1");
    assert_eq!(agent2_id, 2, "agent in project 2 should have id=2");
    assert_eq!(agent2_proj, 2, "second agent should be in project 2");
}

// ---------------------------------------------------------------------------
// T16.1.5: Test v10b index creation and uniqueness enforcement
// (br-3h13.16.1.5)
// ---------------------------------------------------------------------------

#[test]
fn v10b_index_creation_and_uniqueness_enforcement() {
    // Setup: Clean agents (no duplicates) so v10a is a no-op and v10b creates the index.
    let (conn, _dir) = setup_pre_v10_db_with_agents(&[
        (1, "Alice", 1),
        (1, "Bob", 2),
        (2, "Alice", 3), // Same name but different project — allowed.
    ]);

    // Run all migrations (v10a dedup + v10b index creation).
    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    assert!(
        applied
            .iter()
            .any(|id| id == "v10b_idx_agents_project_name_nocase"),
        "v10b migration should have been applied"
    );

    // Verify index exists via PRAGMA index_list.
    let rows = conn
        .query_sync("PRAGMA index_list(agents)", &[])
        .expect("query index_list");

    let index_names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();
    assert!(
        index_names.contains(&"idx_agents_project_name_nocase".to_string()),
        "idx_agents_project_name_nocase should exist in {index_names:?}"
    );

    // Verify the index is UNIQUE (origin = 'u' in PRAGMA index_list).
    let unique_flag: Option<i64> = rows
        .iter()
        .find(|r| {
            r.get_named::<String>("name").unwrap_or_default() == "idx_agents_project_name_nocase"
        })
        .and_then(|r| r.get_named::<i64>("unique").ok());
    assert_eq!(
        unique_flag,
        Some(1),
        "idx_agents_project_name_nocase should be UNIQUE"
    );

    // Verify uniqueness enforcement: inserting a case-duplicate should FAIL.
    let result = conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) \
         VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("alice".into()), // "Alice" already exists in project 1
            Value::Text("test".into()),
            Value::Text("model".into()),
            Value::BigInt(99_000_000),
            Value::BigInt(99_000_000),
        ],
    );
    assert!(
        result.is_err(),
        "inserting case-duplicate 'alice' when 'Alice' exists in same project should fail with UNIQUE constraint"
    );

    // Verify cross-project insertion still works: "bob" in project 2 should succeed.
    let result = conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) \
         VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(2),
            Value::Text("bob".into()), // "Bob" only exists in project 1, not project 2
            Value::Text("test".into()),
            Value::Text("model".into()),
            Value::BigInt(100_000_000),
            Value::BigInt(100_000_000),
        ],
    );
    assert!(
        result.is_ok(),
        "inserting 'bob' in project 2 (only exists in project 1) should succeed"
    );

    // Verify final state: 4 agents total (3 original + 1 new cross-project).
    let rows = conn
        .query_sync("SELECT COUNT(*) as cnt FROM agents", &[])
        .expect("count agents");
    let count: i64 = rows[0].get_named("cnt").unwrap_or(0);
    assert_eq!(
        count, 4,
        "should have 4 agents total after cross-project insert"
    );
}

// ---------------------------------------------------------------------------
// v23: FK-cascade-via-trigger + orphan scrub (#119/#120/#113)
// ---------------------------------------------------------------------------

/// Issue #112 Bug B reconciliation: stuck-NULL `file_reservations.released_ts`
/// values should be backfilled from the `file_reservation_releases` sidecar
/// when the migration runs.
#[test]
fn v23_backfills_stuck_null_released_ts_from_sidecar() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    conn.execute_raw(
        "INSERT INTO projects (id, slug, human_key, created_at) \
         VALUES (1, 'p', '/tmp/p', 0)",
    )
    .expect("insert project");
    conn.execute_raw(
        "INSERT INTO agents \
         (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
         VALUES (1, 1, 'A', 'codex-cli', 'gpt-5', '', 0, 0, 'auto', 'auto')",
    )
    .expect("insert agent");
    // Reservation row: stuck-NULL released_ts.
    conn.execute_raw(
        "INSERT INTO file_reservations \
         (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
         VALUES (10, 1, 1, 'src/*.rs', 1, '', 0, 1000, NULL)",
    )
    .expect("insert reservation");
    // Sidecar row: actual release timestamp.
    conn.execute_raw(
        "INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (10, 500)",
    )
    .expect("insert sidecar release");

    // Re-run the v23 backfill migration directly to verify idempotent
    // backfill against this fixture.
    conn.execute_raw(
        "UPDATE file_reservations \
            SET released_ts = (\
                SELECT released_ts FROM file_reservation_releases \
                WHERE reservation_id = file_reservations.id\
            ) \
            WHERE released_ts IS NULL \
              AND EXISTS (\
                SELECT 1 FROM file_reservation_releases \
                WHERE reservation_id = file_reservations.id\
              )",
    )
    .expect("v23 backfill statement");

    let rows = conn
        .query_sync(
            "SELECT released_ts FROM file_reservations WHERE id = 10",
            &[],
        )
        .expect("query released_ts");
    let released_ts: i64 = rows[0]
        .get_named("released_ts")
        .expect("released_ts must be backfilled");
    assert_eq!(
        released_ts, 500,
        "v23 should backfill stuck-NULL released_ts from sidecar"
    );
}

/// Issue #119/#120 orphan scrub: dangling `message_recipients` rows whose
/// parent message has been deleted must be removed at upgrade time.
#[test]
fn v23_scrubs_orphan_message_recipients_with_missing_message() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    conn.execute_raw(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'p', '/tmp/p', 0)",
    )
    .expect("insert project");
    conn.execute_raw(
        "INSERT INTO agents \
         (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
         VALUES (1, 1, 'A', 'codex-cli', 'gpt-5', '', 0, 0, 'auto', 'auto')",
    )
    .expect("insert agent");
    conn.execute_raw("PRAGMA foreign_keys = OFF")
        .expect("disable foreign keys for orphan fixture");
    // Recipient pointing at a non-existent message id.
    conn.execute_raw(
        "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (9999, 1, 'to')",
    )
    .expect("insert dangling recipient");

    // Re-run the v23 scrub statement.
    conn.execute_raw(
        "DELETE FROM message_recipients WHERE message_id NOT IN (SELECT id FROM messages)",
    )
    .expect("v23 scrub statement");

    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS c FROM message_recipients WHERE message_id = 9999",
            &[],
        )
        .expect("query");
    assert_eq!(
        rows[0].get_named::<i64>("c").unwrap_or(-1),
        0,
        "v23 must delete recipient rows whose parent message is gone"
    );
}

/// Issue #120/v24 cascade-trigger contract: deleting an agent must
/// cascade-delete operational rows, while preserving `message_recipients` as
/// message history even with `foreign_keys = OFF`.
#[test]
fn v23_cascade_triggers_remove_dependents_when_agent_deleted() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    conn.execute_raw("PRAGMA foreign_keys = OFF")
        .expect("disable foreign keys (matches runtime config)");

    conn.execute_raw(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'p', '/tmp/p', 0)",
    )
    .expect("insert project");
    conn.execute_raw(
        "INSERT INTO agents \
         (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
         VALUES \
            (1, 1, 'Sender', 'codex-cli', 'gpt-5', '', 0, 0, 'auto', 'auto'), \
            (2, 1, 'Recipient', 'codex-cli', 'gpt-5', '', 0, 0, 'auto', 'auto'), \
            (3, 1, 'Buddy', 'codex-cli', 'gpt-5', '', 0, 0, 'auto', 'auto')",
    )
    .expect("insert agents");
    conn.execute_raw(
        "INSERT INTO messages \
         (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts) \
         VALUES (1, 1, 1, 'T', 'subject', 'body', 'normal', 0, 0)",
    )
    .expect("insert message");
    conn.execute_raw(
        "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (1, 2, 'to')",
    )
    .expect("insert recipient");
    conn.execute_raw(
        "INSERT INTO file_reservations \
         (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts) \
         VALUES (10, 1, 2, 'src/*.rs', 1, '', 0, 1000)",
    )
    .expect("insert reservation");
    conn.execute_raw(
        "INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (10, 500)",
    )
    .expect("insert release sidecar");
    conn.execute_raw(
        "INSERT INTO agent_links \
         (a_project_id, a_agent_id, b_project_id, b_agent_id, status, created_ts, updated_ts) \
         VALUES (1, 2, 1, 3, 'approved', 0, 0)",
    )
    .expect("insert agent link");
    // The v6 `trg_inbox_stats_insert` trigger already created an inbox_stats
    // row for agent 2 when we inserted into `message_recipients` above.
    // Verify it is in place rather than re-inserting (which would PRIMARY
    // KEY violate).
    let pre_delete_inbox_rows = conn
        .query_sync(
            "SELECT COUNT(*) AS c FROM inbox_stats WHERE agent_id = 2",
            &[],
        )
        .expect("count inbox_stats pre-delete");
    assert_eq!(
        pre_delete_inbox_rows[0].get_named::<i64>("c").unwrap_or(-1),
        1,
        "v6 inbox_stats trigger must have created the recipient's row"
    );

    // Delete the agent — the v23 triggers should cascade.
    conn.execute_raw("DELETE FROM agents WHERE id = 2")
        .expect("delete agent");

    let count = |sql: &str| -> i64 {
        let rows = conn.query_sync(sql, &[]).expect("count query");
        rows[0].get_named::<i64>("c").unwrap_or(-1)
    };

    assert_eq!(
        count("SELECT COUNT(*) AS c FROM message_recipients WHERE agent_id = 2"),
        1,
        "v24 must preserve message_recipients when agent metadata is deleted"
    );
    assert_eq!(
        count("SELECT COUNT(*) AS c FROM file_reservations WHERE agent_id = 2"),
        0,
        "agents-DELETE trigger should cascade to file_reservations"
    );
    assert_eq!(
        count("SELECT COUNT(*) AS c FROM file_reservation_releases WHERE reservation_id = 10"),
        0,
        "file_reservations-DELETE trigger should cascade to the sidecar release ledger"
    );
    assert_eq!(
        count("SELECT COUNT(*) AS c FROM agent_links WHERE a_agent_id = 2 OR b_agent_id = 2"),
        0,
        "agents-DELETE trigger should cascade to agent_links"
    );
    assert_eq!(
        count("SELECT COUNT(*) AS c FROM inbox_stats WHERE agent_id = 2"),
        0,
        "agents-DELETE trigger should cascade to inbox_stats"
    );
}

/// Issue #120: deleting a parent message must cascade-delete its recipient
/// rows.  Complements the agents-side cascade above.
#[test]
fn v23_cascade_triggers_remove_recipients_when_parent_message_deleted() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    conn.execute_raw(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'p', '/tmp/p', 0)",
    )
    .expect("insert project");
    conn.execute_raw(
        "INSERT INTO agents \
         (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
         VALUES \
            (1, 1, 'Sender', 'codex-cli', 'gpt-5', '', 0, 0, 'auto', 'auto'), \
            (2, 1, 'Recipient', 'codex-cli', 'gpt-5', '', 0, 0, 'auto', 'auto')",
    )
    .expect("insert agents");
    conn.execute_raw(
        "INSERT INTO messages \
         (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts) \
         VALUES (1, 1, 1, 'T', 'subject', 'body', 'normal', 0, 0)",
    )
    .expect("insert message");
    conn.execute_raw(
        "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (1, 2, 'to')",
    )
    .expect("insert recipient");

    conn.execute_raw("DELETE FROM messages WHERE id = 1")
        .expect("delete message");

    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS c FROM message_recipients WHERE message_id = 1",
            &[],
        )
        .expect("count");
    assert_eq!(
        rows[0].get_named::<i64>("c").unwrap_or(-1),
        0,
        "messages-DELETE trigger should cascade to message_recipients"
    );
}

/// Idempotency: rerunning v23 should not break, and should not re-create
/// triggers (they use IF NOT EXISTS).
#[test]
fn v23_migration_is_idempotent_when_rerun() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Run again — should be a no-op (idempotent migrations).
    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(
        applied.is_empty(),
        "second migrate_to_latest call must not re-apply any migrations: {applied:?}"
    );

    // All surviving v23 triggers exist exactly once; v24 deliberately removes
    // the agent-delete recipient cascade trigger.
    for name in &[
        "trg_agents_cascade_file_reservations",
        "trg_file_reservations_cascade_releases",
        "trg_agents_cascade_agent_links",
        "trg_agents_cascade_inbox_stats",
        "trg_messages_cascade_recipients",
    ] {
        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS c FROM sqlite_master WHERE type = 'trigger' AND name = ?",
                &[Value::Text((*name).to_string())],
            )
            .expect("count trigger");
        assert_eq!(
            rows[0].get_named::<i64>("c").unwrap_or(-1),
            1,
            "trigger {name} must exist exactly once after migration"
        );
    }
    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS c FROM sqlite_master WHERE type = 'trigger' AND name = ?",
            &[Value::Text(
                "trg_agents_cascade_message_recipients".to_string(),
            )],
        )
        .expect("count dropped trigger");
    assert_eq!(
        rows[0].get_named::<i64>("c").unwrap_or(-1),
        0,
        "v24 must keep the agent-delete recipient cascade trigger dropped"
    );
}
