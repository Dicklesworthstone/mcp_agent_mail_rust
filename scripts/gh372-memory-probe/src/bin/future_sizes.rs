//! Diagnostic only: report future layout without polling, opening a database,
//! or constructing the futures on the calling thread's stack.
use std::future::Future;

fn report<F: Future>(name: &str, _make: impl FnOnce() -> F) {
    println!("{name}: {} bytes", std::mem::size_of::<F>());
}

fn main() {
    println!(
        "Connection: {} bytes",
        std::mem::size_of::<fsqlite::Connection>()
    );
    report("Connection::open", || fsqlite::Connection::open(":memory:"));
    report("Connection::open_with_env", || {
        fsqlite::Connection::open_with_env(":memory:", fsqlite::ConnectionEnv::default())
    });
    report("Connection::open_schema_only", || {
        fsqlite::Connection::open_schema_only(":memory:")
    });
    report("Connection::open_existing_with_env", || {
        fsqlite::Connection::open_existing_with_env(":memory:", fsqlite::ConnectionEnv::default())
    });
    report(
        "Connection::open_schema_only_with_wal_index_recovery_and_env",
        || {
            fsqlite::Connection::open_schema_only_with_wal_index_recovery_and_env(
                ":memory:",
                fsqlite::ConnectionEnv::default(),
            )
        },
    );
    report("compat::open_with_flags_with_env", || {
        fsqlite::compat::open_with_flags_with_env(
            ":memory:",
            fsqlite::compat::OpenFlags::SQLITE_OPEN_READ_ONLY,
            fsqlite::ConnectionEnv::default(),
        )
    });
    // These closures are used only for their return types and are never called.
    let conn: Option<fsqlite::Connection> = None;
    let conn_ref = || conn.as_ref().expect("layout-only closure must never run");
    report("Connection::prepare", || conn_ref().prepare("SELECT 1"));
    report("Connection::query", || conn_ref().query("SELECT 1"));
    report("Connection::query_with_params", || {
        conn_ref().query_with_params("SELECT 1 WHERE 1 = ?", &[])
    });
    report("Connection::query_with_params_for_each", || {
        conn_ref().query_with_params_for_each("SELECT 1 WHERE 1 = ?", &[], |_| Ok(()))
    });
    report("Connection::query_row", || conn_ref().query_row("SELECT 1"));
    report("Connection::query_row_with_params", || {
        conn_ref().query_row_with_params("SELECT 1 WHERE 1 = ?", &[])
    });
    report("Connection::execute", || conn_ref().execute("SELECT 1"));
    report("Connection::execute_with_params", || {
        conn_ref().execute_with_params("SELECT 1 WHERE 1 = ?", &[])
    });
    report("Connection::execute_batch", || {
        conn_ref().execute_batch("SELECT 1")
    });
    report(
        "Connection::execute_many_with_params_skip_statement_savepoint_in_explicit_txn",
        || {
            conn_ref()
                .execute_many_with_params_skip_statement_savepoint_in_explicit_txn("SELECT ?", &[])
        },
    );
    report("Connection::begin_transaction", || {
        conn_ref().begin_transaction()
    });
    report("Connection::commit_transaction", || {
        conn_ref().commit_transaction()
    });
    report("Connection::rollback_transaction", || {
        conn_ref().rollback_transaction()
    });
    report("Connection::close", || {
        Option::<fsqlite::Connection>::None
            .expect("layout-only closure must never run")
            .close()
    });
    report("Connection::close_without_checkpoint", || {
        Option::<fsqlite::Connection>::None
            .expect("layout-only closure must never run")
            .close_without_checkpoint()
    });
}
