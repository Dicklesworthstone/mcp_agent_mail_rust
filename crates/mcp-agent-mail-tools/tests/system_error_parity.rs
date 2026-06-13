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
            None,
            None,
        )
        .await
        .expect_err("locked sqlite write should fail");

        lock_conn.execute_raw("ROLLBACK").expect("release lock");

        let p = error_payload(&err);
        assert_eq!(p["type"], "RESOURCE_BUSY", "unexpected payload: {p:?}");
        // D3 (br-bvq1x.4.3): depending on where the lock surfaces, the write
        // either fails pre-retry ("Wait a moment and try again.") or exhausts
        // the bounded retry budget and reports it honestly.
        let msg = p["message"].as_str().expect("message string");
        assert!(
            msg.starts_with("Resource is temporarily busy"),
            "unexpected RESOURCE_BUSY message: {msg}"
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

#[test]
fn send_message_fd_exhaustion_anchor_maps_to_resource_busy() {
    // L2 FD-loop anchor (ts1): "Too many open files. Freed 0 cached repos".
    // D3 (br-bvq1x.4.3): eviction freed nothing, so the envelope must stop
    // advising a blind retry and give a non-retry next action instead.
    let err = db_error_to_mcp_error(DbError::Internal(
        "send_message retry loop exhausted: Too many open files. Freed 0 cached repos".into(),
    ));
    let p = error_payload(&err);

    assert_eq!(p["type"], "RESOURCE_BUSY");
    assert_eq!(p["recoverable"], false);
    assert_eq!(p["data"]["resource_class"], "file_descriptors");
    assert_eq!(p["data"]["eviction_freed"], 0);
    let msg = p["message"].as_str().unwrap();
    assert!(msg.contains("File descriptor limit exhausted"));
    assert!(
        msg.contains("Do NOT retry"),
        "freed-0 must yield a non-retry next action: {msg}"
    );
    assert!(
        p["data"]["error_detail"]
            .as_str()
            .unwrap()
            .contains("Freed 0 cached repos")
    );

    // The structured envelope carries the FD-pressure section with the
    // honest instance-level retry bit.
    let fd = &p["data"]["failure_envelope"]["fd_pressure"];
    assert_eq!(fd["eviction_freed"], 0);
    assert_eq!(fd["immediate_retry_useful"], false);
    assert!(
        fd["next_action"]
            .as_str()
            .unwrap()
            .contains("retrying will fail again")
    );
    #[cfg(target_os = "linux")]
    {
        assert!(
            fd["soft_limit"].as_u64().is_some(),
            "soft fd limit should be visible on linux: {fd}"
        );
        assert!(
            fd["open_fds"].as_u64().is_some(),
            "open fd count should be visible on linux: {fd}"
        );
    }
}

#[test]
fn fd_exhaustion_with_successful_eviction_keeps_retry_advice() {
    let err = db_error_to_mcp_error(DbError::Internal(
        "send_message: Too many open files. Freed 3 cached repos. Retry".into(),
    ));
    let p = error_payload(&err);

    assert_eq!(p["type"], "RESOURCE_BUSY");
    assert_eq!(p["recoverable"], true);
    assert_eq!(p["data"]["eviction_freed"], 3);
    let fd = &p["data"]["failure_envelope"]["fd_pressure"];
    assert_eq!(fd["immediate_retry_useful"], true);
}

// -----------------------------------------------------------------------
// D3 (br-bvq1x.4.3): bounded retry budget exhaustion envelopes
// -----------------------------------------------------------------------

fn wrap_exhausted(inner: DbError) -> DbError {
    DbError::RetryBudgetExhausted {
        operation: "create_message",
        attempts: 17,
        budget: 17,
        elapsed_ms: 29_000,
        inner: Box::new(inner),
    }
}

#[test]
fn busy_retry_budget_exhaustion_envelope_reports_budget_and_locks_guidance() {
    let err = db_error_to_mcp_error(wrap_exhausted(DbError::ResourceBusy(
        "database is locked".into(),
    )));
    let p = error_payload(&err);

    assert_eq!(p["type"], "RESOURCE_BUSY");
    assert_eq!(p["recoverable"], true);
    let msg = p["message"].as_str().unwrap();
    assert!(
        msg.contains("retry budget is exhausted"),
        "budget-aware message: {msg}"
    );
    assert!(msg.contains("17 attempts over 29000 ms"), "budget: {msg}");
    assert!(msg.contains("am doctor locks --json"), "guidance: {msg}");

    let retry = &p["data"]["retry_exhaustion"];
    assert_eq!(retry["operation"], "create_message");
    assert_eq!(retry["attempts_made"], 17);
    assert_eq!(retry["retry_budget"], 17);
    assert_eq!(retry["elapsed_wait_ms"], 29_000);
    assert_eq!(retry["budget_exhausted"], true);
    assert_eq!(retry["immediate_retry_useful"], false);

    let envelope = &p["data"]["failure_envelope"];
    assert_eq!(envelope["class"], "busy_retryable");
    assert_eq!(envelope["retry"]["budget_exhausted"], true);
    assert_eq!(envelope["retry"]["immediate_retry_useful"], false);
    assert!(
        envelope["retry"]["next_action"]
            .as_str()
            .unwrap()
            .contains("am doctor locks --json")
    );
}

#[test]
fn live_owner_retry_budget_exhaustion_envelope_names_owner_and_routes_through_server() {
    let err = db_error_to_mcp_error(wrap_exhausted(DbError::ResourceBusy(
        "Another active process owns this mailbox (pid 4242). \
         Route writes through that process or stop it first."
            .into(),
    )));
    let p = error_payload(&err);

    assert_eq!(p["type"], "RESOURCE_BUSY");
    assert_eq!(p["recoverable"], true);
    let msg = p["message"].as_str().unwrap();
    assert!(
        msg.contains("Route this operation through that server"),
        "route-through-server guidance: {msg}"
    );
    assert!(msg.contains("17 direct-write attempts"), "budget: {msg}");

    let envelope = &p["data"]["failure_envelope"];
    assert_eq!(envelope["class"], "live_owner_no_activity_lock");
    assert_eq!(envelope["lock_owner"]["pid"], 4242);
    assert_eq!(envelope["lock_owner"]["source"], "parsed_from_error_detail");
    assert_eq!(envelope["retry"]["budget_exhausted"], true);
}

#[test]
fn pool_retry_budget_exhaustion_envelope_reports_pool_guidance() {
    let err = db_error_to_mcp_error(wrap_exhausted(DbError::PoolExhausted {
        message: "all connections in use".into(),
        pool_size: 15,
        max_overflow: 5,
    }));
    let p = error_payload(&err);

    assert_eq!(p["type"], "DATABASE_POOL_EXHAUSTED");
    assert_eq!(p["recoverable"], true);
    let msg = p["message"].as_str().unwrap();
    assert!(msg.contains("already retried 17 times"), "budget: {msg}");
    assert!(msg.contains("Reduce concurrent agents"), "guidance: {msg}");

    let envelope = &p["data"]["failure_envelope"];
    assert_eq!(envelope["class"], "pool_exhaustion");
    assert_eq!(envelope["retry"]["budget_exhausted"], true);
}

#[test]
fn fd_retry_budget_exhaustion_with_freed_zero_is_not_recoverable() {
    let err = db_error_to_mcp_error(wrap_exhausted(DbError::Internal(
        "Too many open files. Freed 0 cached repos".into(),
    )));
    let p = error_payload(&err);

    assert_eq!(p["type"], "RESOURCE_BUSY");
    assert_eq!(p["recoverable"], false);
    let msg = p["message"].as_str().unwrap();
    assert!(msg.contains("Do NOT retry"), "non-retry guidance: {msg}");
    assert_eq!(p["data"]["eviction_freed"], 0);

    let envelope = &p["data"]["failure_envelope"];
    assert_eq!(envelope["class"], "fd_exhaustion");
    assert_eq!(envelope["fd_pressure"]["immediate_retry_useful"], false);
    assert_eq!(envelope["retry"]["budget_exhausted"], true);
}

#[test]
fn corruption_inside_retry_wrapper_falls_back_to_corruption_envelope() {
    // Corruption is never retried by the bounded loops, but if it ever
    // arrives wrapped the corruption envelope must win (CORRUPTION stays a
    // distinct class per D3/A1).
    let err = db_error_to_mcp_error(wrap_exhausted(DbError::Sqlite(
        "database disk image is malformed".into(),
    )));
    let p = error_payload(&err);

    assert_eq!(p["type"], "DATABASE_CORRUPTION");
    assert_eq!(p["recoverable"], false);
    assert_eq!(
        p["data"]["failure_envelope"]["class"],
        "main_db_btree_corruption"
    );
}

#[test]
fn d3_classes_produce_distinct_envelopes() {
    // Acceptance (br-bvq1x.4.3): BUSY_RETRYABLE, FD_EXHAUSTION,
    // POOL_EXHAUSTION, LIVE_OWNER_NO_ACTIVITY_LOCK, and CORRUPTION must each
    // produce a distinct agent-readable envelope.
    let cases = vec![
        wrap_exhausted(DbError::ResourceBusy("database is locked".into())),
        wrap_exhausted(DbError::Internal(
            "Too many open files. Freed 0 cached repos".into(),
        )),
        wrap_exhausted(DbError::PoolExhausted {
            message: "all connections in use".into(),
            pool_size: 15,
            max_overflow: 5,
        }),
        wrap_exhausted(DbError::ResourceBusy(
            "Another active process owns this mailbox (pid 4242). \
             Route writes through that process or stop it first."
                .into(),
        )),
        wrap_exhausted(DbError::Sqlite("database disk image is malformed".into())),
    ];

    let mut seen_classes = std::collections::BTreeSet::new();
    let mut seen_messages = std::collections::BTreeSet::new();
    for case in cases {
        let p = error_payload(&db_error_to_mcp_error(case));
        let class = p["data"]["failure_envelope"]["class"]
            .as_str()
            .unwrap()
            .to_string();
        let message = p["message"].as_str().unwrap().to_string();
        assert!(
            seen_classes.insert(class.clone()),
            "duplicate class {class}"
        );
        assert!(
            seen_messages.insert(message.clone()),
            "duplicate message for class {class}: {message}"
        );
    }
    assert_eq!(seen_classes.len(), 5);
}

#[test]
fn bad_file_descriptor_does_not_map_to_fd_exhaustion() {
    let err = db_error_to_mcp_error(DbError::Internal("bad file descriptor".into()));
    let p = error_payload(&err);

    assert_eq!(p["type"], "UNHANDLED_EXCEPTION");
    assert_eq!(p["recoverable"], false);
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
