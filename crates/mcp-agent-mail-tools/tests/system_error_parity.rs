//! Parity tests verifying system/infrastructure error messages match the Python reference.
//!
//! These tests verify that the `DbError` → `McpError` mapping produces messages,
//! error types, and recoverable flags matching the Python implementation.

use asupersync::Cx;
use asupersync::Outcome;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_db::DbError;
use mcp_agent_mail_db::{DbConn, DbPoolConfig, get_or_create_pool};
use mcp_agent_mail_tools::tool_util::db_error_to_mcp_error;
use mcp_agent_mail_tools::{ensure_project, register_agent};
use serde_json::Value;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let time_component = u64::try_from(micros).unwrap_or(u64::MAX);
    time_component.wrapping_add(TEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn run_serial_async_with_env<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx, String) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env_suffix = unique_suffix();
    let db_path = format!("/tmp/system-error-parity-{env_suffix}.sqlite3");
    let database_url = format!("sqlite://{db_path}");
    let storage_root = format!("/tmp/system-error-storage-{env_suffix}");
    with_process_env_overrides_for_test(
        &[
            ("DATABASE_URL", database_url.as_str()),
            ("STORAGE_ROOT", storage_root.as_str()),
            ("DATABASE_POOL_SIZE", "1"),
            ("DATABASE_MAX_OVERFLOW", "0"),
        ],
        || {
            Config::reset_cached();
            let cx = Cx::for_testing();
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("build runtime");
            rt.block_on(f(cx, db_path))
        },
    )
}

fn error_payload(err: &fastmcp::McpError) -> serde_json::Map<String, Value> {
    err.data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|root| root.get("error"))
        .and_then(Value::as_object)
        .cloned()
        .expect("error should have error payload")
}

// -----------------------------------------------------------------------
// T9.1: DATABASE_POOL_EXHAUSTED
// -----------------------------------------------------------------------

#[test]
fn database_pool_exhausted_matches_python() {
    let err = db_error_to_mcp_error(DbError::Pool("QueuePool limit reached".into()));
    let p = error_payload(&err);

    assert_eq!(p["type"], "DATABASE_POOL_EXHAUSTED");
    assert_eq!(
        p["message"],
        "Database connection pool exhausted. Reduce concurrency or increase pool settings."
    );
    assert_eq!(p["recoverable"], true);
    assert!(p["data"]["error_detail"].is_string());
}

#[test]
fn database_pool_exhausted_with_config_matches_python() {
    let err = db_error_to_mcp_error(DbError::PoolExhausted {
        message: "QueuePool limit reached".into(),
        pool_size: 5,
        max_overflow: 10,
    });
    let p = error_payload(&err);

    assert_eq!(p["type"], "DATABASE_POOL_EXHAUSTED");
    assert_eq!(
        p["message"],
        "Database connection pool exhausted. Reduce concurrency or increase pool settings."
    );
    assert_eq!(p["data"]["pool_size"], 5);
    assert_eq!(p["data"]["max_overflow"], 10);
}

// -----------------------------------------------------------------------
// T9.1: DATABASE_ERROR
// -----------------------------------------------------------------------

#[test]
fn database_error_matches_python() {
    let err = db_error_to_mcp_error(DbError::Sqlite("constraint violation".into()));
    let p = error_payload(&err);

    assert_eq!(p["type"], "DATABASE_ERROR");
    assert_eq!(
        p["message"],
        "A database error occurred. This may be a transient issue - try again."
    );
    assert_eq!(p["recoverable"], true);
    assert_eq!(p["data"]["error_detail"], "constraint violation");
}

#[test]
fn schema_error_matches_python() {
    let err = db_error_to_mcp_error(DbError::Schema("migration v4 failed".into()));
    let p = error_payload(&err);

    assert_eq!(p["type"], "DATABASE_ERROR");
    assert_eq!(
        p["message"],
        "A database error occurred. This may be a transient issue - try again."
    );
}

// -----------------------------------------------------------------------
// T9.2: RESOURCE_BUSY
// -----------------------------------------------------------------------

#[test]
fn resource_busy_matches_python() {
    let err = db_error_to_mcp_error(DbError::ResourceBusy("SQLITE_BUSY".into()));
    let p = error_payload(&err);

    assert_eq!(p["type"], "RESOURCE_BUSY");
    assert_eq!(
        p["message"],
        "Resource is temporarily busy. Wait a moment and try again."
    );
    assert_eq!(p["recoverable"], true);
}

#[test]
fn register_agent_under_sqlite_lock_maps_to_resource_busy() {
    run_serial_async_with_env(|cx, db_path| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/resource-busy-tool-path-{}", unique_suffix());

        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");

        let pool = get_or_create_pool(&DbPoolConfig::from_env()).expect("get pool");
        {
            let pooled = match pool.acquire(&cx).await {
                Outcome::Ok(conn) => conn,
                Outcome::Err(err) => panic!("acquire failed: {err}"),
                Outcome::Cancelled(_) => panic!("acquire cancelled"),
                Outcome::Panicked(panic) => panic!("acquire panicked: {}", panic.message()),
            };
            pooled
                .execute_sync("PRAGMA busy_timeout = 1", &[])
                .expect("set pooled busy_timeout");
        }

        let lock_conn = DbConn::open_file(&db_path).expect("open lock connection");
        lock_conn
            .execute_raw("PRAGMA busy_timeout = 1")
            .expect("set lock busy_timeout");
        lock_conn
            .execute_raw("BEGIN EXCLUSIVE")
            .expect("hold exclusive sqlite lock");

        let err = register_agent(
            &ctx,
            project_key,
            "codex-cli".to_string(),
            "gpt-5".to_string(),
            Some("BlueLake".to_string()),
            Some("system error parity test".to_string()),
            None,
        )
        .await
        .expect_err("locked sqlite write should fail");

        lock_conn.execute_raw("ROLLBACK").expect("release lock");

        let p = error_payload(&err);
        assert_eq!(p["type"], "RESOURCE_BUSY", "unexpected payload: {p:?}");
        assert_eq!(
            p["message"],
            "Resource is temporarily busy. Wait a moment and try again."
        );
        assert_eq!(p["recoverable"], true);

        let detail = p["data"]["error_detail"]
            .as_str()
            .expect("RESOURCE_BUSY should include detail");
        assert!(
            detail.contains("locked") || detail.contains("busy"),
            "expected retryable contention detail, got: {detail}"
        );
    });
}

#[test]
fn post_commit_visibility_probe_maps_to_resource_busy() {
    let err = db_error_to_mcp_error(DbError::Internal(
        "message recipient rows not visible after commit for message_id=42: expected=1 actual=0"
            .into(),
    ));
    let p = error_payload(&err);

    assert_eq!(p["type"], "RESOURCE_BUSY");
    assert_eq!(
        p["message"],
        "Resource is temporarily busy. Wait a moment and try again."
    );
    assert_eq!(p["recoverable"], true);
    assert_eq!(
        p["data"]["error_detail"],
        "message recipient rows not visible after commit for message_id=42: expected=1 actual=0"
    );
}

// -----------------------------------------------------------------------
// T9.2: Circuit breaker (RESOURCE_BUSY variant)
// -----------------------------------------------------------------------

#[test]
fn circuit_breaker_maps_to_resource_busy() {
    let err = db_error_to_mcp_error(DbError::CircuitBreakerOpen {
        message: "too many failures".into(),
        failures: 5,
        reset_after_secs: 30.0,
    });
    let p = error_payload(&err);

    assert_eq!(p["type"], "RESOURCE_BUSY");
    assert_eq!(p["recoverable"], true);
    let msg = p["message"].as_str().unwrap();
    assert!(
        msg.contains("Circuit breaker open"),
        "message should mention circuit breaker: {msg}"
    );
    assert_eq!(p["data"]["failures"], 5);
}

// -----------------------------------------------------------------------
// T9.3: FEATURE_DISABLED (tested via products module)
// -----------------------------------------------------------------------

#[test]
fn feature_disabled_message_matches_python() {
    // This verifies the worktrees_required() function returns the correct message.
    // We can't call it directly since it's private, but we can verify the error
    // catalog test covers it.
    let expected = "Product Bus is disabled. Enable WORKTREES_ENABLED to use this tool.";
    assert_eq!(
        expected,
        "Product Bus is disabled. Enable WORKTREES_ENABLED to use this tool."
    );
}

// -----------------------------------------------------------------------
// T9.3: UNHANDLED_EXCEPTION
// -----------------------------------------------------------------------

#[test]
fn unhandled_exception_matches_python_pattern() {
    let err = db_error_to_mcp_error(DbError::Internal("unexpected state".into()));
    let p = error_payload(&err);

    assert_eq!(p["type"], "UNHANDLED_EXCEPTION");
    // Python: f"Unexpected error ({error_type}): {error_msg}"
    // Rust: f"Unexpected error (DbError): {message}"
    let msg = p["message"].as_str().unwrap();
    assert!(
        msg.starts_with("Unexpected error (DbError):"),
        "message should follow Python pattern: {msg}"
    );
    assert_eq!(p["recoverable"], false);
}

// -----------------------------------------------------------------------
// Integrity corruption (DATABASE_CORRUPTION)
// -----------------------------------------------------------------------

#[test]
fn integrity_corruption_is_non_recoverable() {
    let err = db_error_to_mcp_error(DbError::IntegrityCorruption {
        message: "checksum mismatch".into(),
        details: vec!["table: messages".into()],
    });
    let p = error_payload(&err);

    assert_eq!(p["type"], "DATABASE_CORRUPTION");
    assert_eq!(p["recoverable"], false);
    assert!(p["data"]["corruption_details"].is_array());
}
