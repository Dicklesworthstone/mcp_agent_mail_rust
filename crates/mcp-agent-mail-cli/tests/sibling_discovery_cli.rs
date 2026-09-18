//! Real-process coverage for the explicit discovery maintenance executable.
//! Fixtures use the runtime driver and never pre-seed a suggestion row.

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::{Command, Output};

fn command(database: &Path, storage: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_am-discover-siblings"));
    command
        .env("DATABASE_URL", format!("sqlite:///{}", database.display()))
        .env("STORAGE_ROOT", storage)
        .env("AM_INTERFACE_MODE", "cli");
    command
}

fn report(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "discovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("machine-readable JSON report")
}

#[test]
fn standalone_discovery_persists_hints_and_preserves_review_on_rerun() {
    let directory = tempfile::tempdir()
        .expect("retained runtime fixture")
        .keep();
    let database = directory.join("mailbox.sqlite3");
    let storage = directory.join("archive");
    std::fs::create_dir_all(&storage).unwrap();
    let conn = mcp_agent_mail_db::DbConn::open_file(database.to_string_lossy().as_ref()).unwrap();
    conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
        .unwrap();
    conn.execute_raw(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES \
         (1, 'acme-api', '/work/acme-api', 1), (2, 'acme-web', '/work/acme-web', 1)",
    )
    .unwrap();
    mcp_agent_mail_db::close_db_conn(conn, "discovery CLI fixture");

    let first = report(
        command(&database, &storage)
            .args(["--project-id", "1"])
            .output()
            .unwrap(),
    );
    assert_eq!(first["refresh"]["written"], 1);
    assert_eq!(first["refresh"]["suggestions"][0]["project_a_id"], 1);
    assert_eq!(first["refresh"]["suggestions"][0]["project_b_id"], 2);
    assert!(
        first["refresh"]["suggestions"][0]["score"]
            .as_f64()
            .unwrap()
            >= mcp_agent_mail_db::queries::PROJECT_SIBLING_MIN_SUGGESTION_SCORE
    );

    let second = report(command(&database, &storage).output().unwrap());
    assert_eq!(second["refresh"]["written"], 0);
    let conn = mcp_agent_mail_db::DbConn::open_file(database.to_string_lossy().as_ref()).unwrap();
    let before = conn
        .query_sync(
            "SELECT id, created_ts, evaluated_ts FROM project_sibling_suggestions",
            &[],
        )
        .unwrap();
    assert_eq!(before.len(), 1);
    let id = before[0].get_named::<i64>("id").unwrap();
    let created = before[0].get_named::<i64>("created_ts").unwrap();
    conn.execute_raw(
        "UPDATE project_sibling_suggestions SET status = 'confirmed', confirmed_ts = 7, \
         evaluated_ts = 1",
    )
    .unwrap();
    mcp_agent_mail_db::close_db_conn(conn, "discovery CLI review fixture");

    let reviewed = report(command(&database, &storage).output().unwrap());
    assert_eq!(reviewed["refresh"]["written"], 0);
    let missing = command(&database, &storage)
        .args(["--project-id", "999999"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("999999"));

    let conn = mcp_agent_mail_db::DbConn::open_file(database.to_string_lossy().as_ref()).unwrap();
    let after = conn
        .query_sync("SELECT * FROM project_sibling_suggestions", &[])
        .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].get_named::<i64>("id").unwrap(), id);
    assert_eq!(after[0].get_named::<i64>("created_ts").unwrap(), created);
    assert_eq!(after[0].get_named::<i64>("confirmed_ts").unwrap(), 7);
    assert_eq!(after[0].get_named::<String>("status").unwrap(), "confirmed");
    assert_eq!(after[0].get_named::<i64>("evaluated_ts").unwrap(), 1);
    mcp_agent_mail_db::close_db_conn(conn, "discovery CLI verification");
}

#[test]
fn help_and_invalid_arguments_never_materialize_a_mailbox() {
    let directory = tempfile::tempdir()
        .expect("retained CLI parsing fixture")
        .keep();
    let database = directory.join("must-not-be-created.sqlite3");
    let storage = directory.join("must-not-be-created-archive");
    let help = command(&database, &storage).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--project-id"));
    let invalid = command(&database, &storage)
        .args(["--project-id", "0"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(!database.exists());
    assert!(!storage.exists());
}
