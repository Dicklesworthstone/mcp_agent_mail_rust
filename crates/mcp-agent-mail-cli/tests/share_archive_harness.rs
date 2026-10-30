//! br-3ibsu: Native harness adapter for share/archive-heavy E2E suites.
//!
//! Current phase: managed adapter.
//! - Native runner entrypoint is Rust (`cargo test -p mcp-agent-mail-cli --test it share_archive_harness::`).
//! - Scenario execution delegates to shell suites (`share`, `share_verify_live`, `archive`).
//! - Harness captures deterministic metadata and copies legacy artifacts.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlmodel_core::Value as SqlValue;

#[derive(Debug)]
struct CommandOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    duration_ms: u64,
    timed_out: bool,
}

#[derive(Debug, Serialize)]
struct HarnessSummary {
    suite: String,
    runner: String,
    adapter_mode: String,
    strict_mode: bool,
    generated_at: String,
    duration_ms: u64,
    passed: bool,
    adapter_script: String,
    adapter_exit_code: Option<i32>,
    adapter_timed_out: bool,
    adapter_duration_ms: u64,
    adapter_stdout_path: String,
    adapter_stderr_path: String,
    legacy_artifacts_root: String,
    new_legacy_run_paths: Vec<String>,
    copied_legacy_run_path: Option<String>,
    notes: Vec<String>,
}

const DEFAULT_HARNESS_SUITE: &str = "share";

#[derive(Clone, Copy, Debug)]
struct HarnessSuiteSpec {
    suite_name: &'static str,
    adapter_script: &'static str,
    legacy_artifacts_subdir: &'static str,
}

fn bool_env(var_name: &str) -> bool {
    std::env::var(var_name).is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

fn selected_suite_name() -> String {
    std::env::var("AM_SHARE_ARCHIVE_HARNESS_SUITE")
        .unwrap_or_else(|_| DEFAULT_HARNESS_SUITE.to_string())
}

fn suite_spec_for(name: &str) -> HarnessSuiteSpec {
    match name {
        "share" => HarnessSuiteSpec {
            suite_name: "share",
            adapter_script: "tests/e2e/test_share.sh",
            legacy_artifacts_subdir: "share",
        },
        "share_verify_live" => HarnessSuiteSpec {
            suite_name: "share_verify_live",
            adapter_script: "tests/e2e/test_share_verify_live.sh",
            legacy_artifacts_subdir: "share_verify_live",
        },
        "archive" => HarnessSuiteSpec {
            suite_name: "archive",
            adapter_script: "tests/e2e/test_archive.sh",
            legacy_artifacts_subdir: "archive",
        },
        _ => panic!(
            "unsupported AM_SHARE_ARCHIVE_HARNESS_SUITE={name}; expected one of: share, share_verify_live, archive"
        ),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf()
}

fn harness_artifacts_root(suite_name: &str) -> PathBuf {
    if let Ok(override_root) = std::env::var("AM_SHARE_ARCHIVE_ARTIFACT_DIR") {
        return PathBuf::from(override_root).join(suite_name);
    }
    repo_root().join("tests/artifacts/cli").join(suite_name)
}

fn legacy_artifacts_root(subdir: &str) -> PathBuf {
    repo_root().join("tests/artifacts").join(subdir)
}

fn list_run_directories(root: &Path) -> BTreeSet<String> {
    let mut runs = BTreeSet::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return runs,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            runs.insert(name.to_string());
        }
    }
    runs
}

fn copy_directory_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_directory_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(src_path, dst_path)?;
        }
    }
    Ok(())
}

fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> CommandOutcome {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn command");
    let mut stdout = child.stdout.take().expect("capture adapter stdout");
    let mut stderr = child.stderr.take().expect("capture adapter stderr");
    let stdout_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).expect("read adapter stdout");
        buf
    });
    let stderr_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).expect("read adapter stderr");
        buf
    });
    let mut timed_out = false;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().expect("wait after adapter timeout");
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("try_wait failed: {error}"),
        }
    };

    let stdout = stdout_reader.join().expect("join adapter stdout reader");
    let stderr = stderr_reader.join().expect("join adapter stderr reader");
    let mut stderr = String::from_utf8_lossy(&stderr).to_string();
    if timed_out {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "adapter timed out after {}ms",
            timeout.as_millis()
        ));
    }

    CommandOutcome {
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr,
        duration_ms: start.elapsed().as_millis() as u64,
        timed_out,
    }
}

#[test]
fn native_share_archive_transport_gate() {
    let suite_name = selected_suite_name();
    let suite_spec = suite_spec_for(&suite_name);
    let start_instant = Instant::now();
    let run_root = harness_artifacts_root(suite_spec.suite_name).join(format!(
        "{}_{}",
        Utc::now().format("%Y%m%d_%H%M%S%.3fZ"),
        std::process::id()
    ));
    fs::create_dir_all(&run_root).expect("create run root");
    eprintln!(
        "share/archive harness artifact root (suite={}): {}",
        suite_spec.suite_name,
        run_root.display()
    );

    let strict_mode = bool_env("AM_E2E_SHARE_ARCHIVE_REQUIRE_PASS");
    let legacy_root = legacy_artifacts_root(suite_spec.legacy_artifacts_subdir);
    fs::create_dir_all(&legacy_root).expect("create legacy artifact root");
    let before_runs = list_run_directories(&legacy_root);

    let repo = repo_root();
    let mut cmd = Command::new("bash");
    cmd.current_dir(&repo)
        .arg(suite_spec.adapter_script)
        .env("E2E_PROJECT_ROOT", &repo)
        .env(
            "E2E_SERVER_START_TIMEOUT_SECONDS",
            std::env::var("E2E_SERVER_START_TIMEOUT_SECONDS").unwrap_or_else(|_| "60".to_string()),
        )
        .env(
            "AM_E2E_KEEP_TMP",
            std::env::var("AM_E2E_KEEP_TMP").unwrap_or_else(|_| "1".to_string()),
        );

    let timeout_secs = std::env::var("AM_SHARE_ARCHIVE_ADAPTER_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(1800);
    let outcome = run_with_timeout(&mut cmd, Duration::from_secs(timeout_secs));

    let stdout_path = run_root.join("adapter.stdout.log");
    let stderr_path = run_root.join("adapter.stderr.log");
    fs::write(&stdout_path, &outcome.stdout).expect("write adapter stdout");
    fs::write(&stderr_path, &outcome.stderr).expect("write adapter stderr");

    let after_runs = list_run_directories(&legacy_root);
    let new_runs = after_runs
        .difference(&before_runs)
        .cloned()
        .collect::<Vec<String>>();

    let copied_legacy_run_path = new_runs.last().and_then(|run_name| {
        let src = legacy_root.join(run_name);
        let dst = run_root
            .join("subsuite")
            .join(suite_spec.suite_name)
            .join(run_name);
        copy_directory_recursive(&src, &dst).ok()?;
        Some(dst.display().to_string())
    });

    let passed = outcome.exit_code == Some(0) && !outcome.timed_out;
    let mut notes = Vec::new();
    if new_runs.is_empty() {
        notes.push("No new legacy artifact run was detected.".to_string());
    }
    if outcome.timed_out {
        notes.push("Adapter execution timed out.".to_string());
    }
    if outcome.exit_code != Some(0) {
        notes.push(format!(
            "Adapter exit code was {:?}, expected 0.",
            outcome.exit_code
        ));
    }

    let summary = HarnessSummary {
        suite: suite_spec.suite_name.to_string(),
        runner: "native".to_string(),
        adapter_mode: "shell_managed".to_string(),
        strict_mode,
        generated_at: Utc::now().to_rfc3339(),
        duration_ms: start_instant.elapsed().as_millis() as u64,
        passed,
        adapter_script: suite_spec.adapter_script.to_string(),
        adapter_exit_code: outcome.exit_code,
        adapter_timed_out: outcome.timed_out,
        adapter_duration_ms: outcome.duration_ms,
        adapter_stdout_path: stdout_path.display().to_string(),
        adapter_stderr_path: stderr_path.display().to_string(),
        legacy_artifacts_root: legacy_root.display().to_string(),
        new_legacy_run_paths: new_runs,
        copied_legacy_run_path,
        notes,
    };
    let summary_path = run_root.join("run_summary.json");
    fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).expect("serialize summary"),
    )
    .expect("write summary");

    if strict_mode {
        assert!(
            passed,
            "{} adapter failed (exit={:?}, timed_out={})\nstdout:\n{}\nstderr:\n{}",
            suite_spec.suite_name,
            outcome.exit_code,
            outcome.timed_out,
            outcome.stdout,
            outcome.stderr
        );
        assert!(
            summary.copied_legacy_run_path.is_some(),
            "{} adapter succeeded but did not produce a new legacy artifact run under {}",
            suite_spec.suite_name,
            legacy_root.display()
        );
    } else if !passed {
        eprintln!(
            "SKIP(non-strict): {} adapter failed but strict mode is disabled; see {}",
            suite_spec.suite_name,
            summary_path.display()
        );
    }
}

// GH#365: real backup CLI scenarios, separate from the shell suite's Git
// message-write checks. Every input is synthetic and every child is isolated.
const ARCHIVE_FIXTURE_TS: i64 = 1_761_393_600_000_000;
const ARCHIVE_FIXTURE_TOKEN: &str = "archive-fixture-registration-token-preserve-me";
const ARCHIVE_PRIVATE_MARKER: &str = "PRIVATE_BACKUP_BODY_DO_NOT_LOG";
const ARCHIVE_SNAPSHOT_ENTRY: &str = "snapshot/mailbox.sqlite3";

struct ArchiveFixture {
    root: tempfile::TempDir,
    database: PathBuf,
    storage: PathBuf,
    messages: usize,
    storage_hashes: BTreeMap<PathBuf, String>,
    database_hash: String,
}

fn archive_file_hash(path: &Path) -> String {
    archive_reader_hash(fs::File::open(path).expect("open fixture for hashing"))
}

fn archive_reader_hash(mut reader: impl Read) -> String {
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let len = reader.read(&mut buffer).expect("hash fixture bytes");
        if len == 0 {
            break;
        }
        hash.update(&buffer[..len]);
    }
    hex::encode(hash.finalize())
}

fn archive_fixture_body(id: i64) -> String {
    format!(
        "{ARCHIVE_PRIVATE_MARKER}: message {id}\n{}\n",
        "Synthetic mailbox recovery, attachment and recipient state. ".repeat(8)
    )
}

fn archive_fixture_attachments() -> String {
    serde_json::json!([{
        "path": "projects/scale-mailbox/attachments/aa/payload.bin",
        "media_type": "application/octet-stream",
        "authorization": "archive-only synthetic attachment credential"
    }])
    .to_string()
}

fn archive_fixture_recipients() -> String {
    serde_json::json!({
        "to": ["BlueLake"], "cc": ["OrangeFox"], "bcc": ["SilverCove"]
    })
    .to_string()
}

fn archive_write_fixture(storage: &Path, relative: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
    let path = storage.join(relative);
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
    fs::write(path, bytes).expect("write archive fixture");
}

impl ArchiveFixture {
    fn new(messages: usize, recovery_mib: usize) -> Self {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .expect("isolated archive fixture");
        fs::write(root.path().join("Cargo.toml"), "[workspace]\n").expect("isolate output root");
        for directory in ["home", "tmp", "config", "data", "state", "runtime"] {
            fs::create_dir(root.path().join(directory)).expect("isolate process directories");
        }
        let database = root.path().join("mailbox.sqlite3");
        let storage = root.path().join("storage_repo");
        fs::create_dir(&storage).expect("create storage");
        // Canonical SQLite seeds an exclusively owned fixture, then closes
        // before runtime admission. The tested CLI uses its real guarded reader.
        let conn = mcp_agent_mail_db::CanonicalDbConn::open_file(database.to_str().unwrap())
            .expect("create synthetic mailbox");
        conn.execute_raw("PRAGMA journal_mode=DELETE; BEGIN IMMEDIATE")
            .expect("seed transaction");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("base schema");
        let inbox_migration = mcp_agent_mail_db::schema::schema_migrations()
            .into_iter()
            .find(|migration| migration.id == "v6_create_inbox_stats")
            .expect("inbox schema");
        conn.execute_raw(&inbox_migration.up)
            .expect("create inbox schema");
        conn.execute_sync(
            "INSERT INTO projects (id,slug,human_key,created_at) VALUES (1,?1,?2,?3)",
            &[
                SqlValue::Text("scale-mailbox".into()),
                SqlValue::Text("/synthetic/archive-scale".into()),
                SqlValue::BigInt(ARCHIVE_FIXTURE_TS),
            ],
        )
        .expect("seed project");
        for (index, name) in ["GreenLake", "BlueLake", "OrangeFox", "SilverCove"]
            .into_iter()
            .enumerate()
        {
            let id = i64::try_from(index + 1).unwrap();
            conn.execute_sync(
                "INSERT INTO agents (id,project_id,name,program,model,inception_ts,last_active_ts,registration_token) \
                 VALUES (?1,1,?2,'archive-test','synthetic',?3,?3,?4)",
                &[SqlValue::BigInt(id), SqlValue::Text(name.into()), SqlValue::BigInt(ARCHIVE_FIXTURE_TS),
                  SqlValue::Text(format!("{ARCHIVE_FIXTURE_TOKEN}-{id}"))],
            ).expect("seed agent");
            archive_write_fixture(
                &storage,
                format!("projects/scale-mailbox/agents/{name}/profile.json"),
                serde_json::json!({"id":id,"name":name,"project_id":1,
                    "program":"archive-test","model":"synthetic",
                    "registration_token":format!("{ARCHIVE_FIXTURE_TOKEN}-{id}")})
                .to_string(),
            );
        }
        archive_write_fixture(
            &storage,
            "projects/scale-mailbox/project.json",
            r#"{"id":1,"slug":"scale-mailbox","human_key":"/synthetic/archive-scale"}"#,
        );
        let attachment_json = archive_fixture_attachments();
        let recipients_json = archive_fixture_recipients();
        for ordinal in 0..messages {
            let id = i64::try_from(ordinal).unwrap() * 3 + 1;
            let body = archive_fixture_body(id);
            conn.execute_sync(
                "INSERT INTO messages \
                 (id,project_id,sender_id,thread_id,topic,subject,body_md,importance,ack_required,created_ts,recipients_json,attachments,archive_metadata_json) \
                 VALUES (?1,1,1,'scale-thread','archive-test',?2,?3,'high',1,?4,?5,?6,'{\"recovery\":\"preserve\"}')",
                &[SqlValue::BigInt(id), SqlValue::Text(format!("Synthetic message {id}")),
                  SqlValue::Text(body.clone()), SqlValue::BigInt(ARCHIVE_FIXTURE_TS+id),
                  SqlValue::Text(recipients_json.clone()), SqlValue::Text(attachment_json.clone())],
            ).expect("seed message");
            for (agent_id, kind) in [(2_i64, "to"), (3, "cc"), (4, "bcc")] {
                conn.execute_sync(
                    "INSERT INTO message_recipients (message_id,agent_id,kind,read_ts,ack_ts) VALUES (?1,?2,?3,?4,?5)",
                    &[SqlValue::BigInt(id), SqlValue::BigInt(agent_id), SqlValue::Text(kind.into()),
                      SqlValue::BigInt(ARCHIVE_FIXTURE_TS+id+1),
                      if agent_id == 2 { SqlValue::BigInt(ARCHIVE_FIXTURE_TS+id+2) } else { SqlValue::Null }],
                ).expect("seed recipient state");
            }
            let filename = format!("2025-10-25T12-00-00Z__synthetic__{id}.md");
            let frontmatter = serde_json::json!({
                "id":id,"project":"/synthetic/archive-scale","from":"GreenLake",
                "to":["BlueLake"],"cc":["OrangeFox"],"bcc":["SilverCove"],
                "subject":format!("Synthetic message {id}"),"created_ts":"2025-10-25T12:00:00Z",
                "thread_id":"scale-thread","importance":"high","ack_required":true,
                "attachments":serde_json::from_str::<serde_json::Value>(&attachment_json).unwrap()});
            let message = format!("---json\n{frontmatter}\n---\n\n{body}");
            for directory in [
                "messages/2025/10",
                "agents/GreenLake/outbox/2025/10",
                "agents/BlueLake/inbox/2025/10",
            ] {
                archive_write_fixture(
                    &storage,
                    format!("projects/scale-mailbox/{directory}/{filename}"),
                    &message,
                );
            }
        }
        conn.execute_raw("COMMIT").expect("commit fixture");
        drop(conn);
        let runtime = mcp_agent_mail_db::DbConn::open_file(database.to_str().unwrap())
            .expect("admit runtime family");
        runtime.close_sync().expect("close runtime writer");
        archive_write_fixture(&storage, ".git/HEAD", "ref: refs/heads/main\n");
        archive_write_fixture(
            &storage,
            ".git/refs/heads/main",
            "0123456789abcdef0123456789abcdef01234567\n",
        );
        archive_write_fixture(
            &storage,
            "projects/scale-mailbox/attachments/aa/payload.bin",
            (0_u8..=255).cycle().take(16 * 1024).collect::<Vec<_>>(),
        );
        for relative in [
            "doctor/forensics/old-run/storage.sqlite3",
            "doctor/reclaimable/old-run/storage.sqlite3",
            "doctor/backlog_dead_letter.jsonl",
            "doctor/reservation-reconcile.json",
            "doctor/backlog_dead_letter/pending.json",
            "doctor/reservation-reconcile/pending.json",
            "doctor/archive-quarantine/unapplied-message.md",
            "doctor/operator-note.txt",
            "doctor/forensics-current/keep.txt",
            "projects/scale-mailbox/doctor/forensics/keep.txt",
            ".archive_backlog/pending.json",
            ".mcp-agent-mail-recovery-receipts/keep.json",
        ] {
            archive_write_fixture(
                &storage,
                relative,
                format!("preserve evidence: {relative}\n"),
            );
        }
        if recovery_mib > 0 {
            // Mixed-entropy bytes, not sparse all-zero files: measure real
            // compression work while keeping the fixture deterministic.
            let mut state = 0x4d59_5df4_d0f3_3173_u64;
            let mut block = vec![0_u8; 1024 * 1024];
            for (index, byte) in block.iter_mut().enumerate() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = if index % 4096 < 1024 {
                    state.to_le_bytes()[0]
                } else {
                    0
                };
            }
            let mut file =
                fs::File::create(storage.join("doctor/forensics/old-run/recovery-payload.bin"))
                    .unwrap();
            for _ in 0..recovery_mib {
                file.write_all(&block).unwrap();
            }
        }
        let storage_hashes = walkdir::WalkDir::new(&storage)
            .into_iter()
            .map(|entry| entry.expect("walk fixture"))
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().strip_prefix(&storage).unwrap().to_path_buf(),
                    archive_file_hash(entry.path()),
                )
            })
            .collect();
        let database_hash = archive_file_hash(&database);
        Self {
            root,
            database,
            storage,
            messages,
            storage_hashes,
            database_hash,
        }
    }

    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(self.root.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.path().join("home"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("XDG_RUNTIME_DIR", self.root.path().join("runtime"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env(
                "DATABASE_URL",
                format!("sqlite:///{}", self.database.display()),
            )
            .env("STORAGE_ROOT", &self.storage)
            .env("AM_INTERFACE_MODE", "cli")
            .env("TUI_ENABLED", "false")
            .env("NO_COLOR", "1");
        command
    }

    fn archives(&self) -> BTreeSet<PathBuf> {
        let directory = self.root.path().join("archived_mailbox_states");
        if !directory.exists() {
            return BTreeSet::new();
        }
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "zip"))
            .collect()
    }

    fn add_durable_extension(&mut self) {
        let conn = mcp_agent_mail_db::DbConn::open_file(self.database.to_str().unwrap()).unwrap();
        conn.execute_raw(
            "BEGIN IMMEDIATE; \
             INSERT INTO db_identity VALUES (0, 'archive-fixture-generation'); \
             CREATE TABLE operator_durable_state (id INTEGER PRIMARY KEY, payload BLOB NOT NULL, note TEXT); \
             CREATE INDEX operator_durable_note ON operator_durable_state(note); \
             CREATE VIEW operator_durable_view AS SELECT id,note FROM operator_durable_state; \
             CREATE TABLE operator_audit (state_id INTEGER NOT NULL); \
             CREATE TRIGGER operator_durable_audit AFTER UPDATE ON operator_durable_state \
             BEGIN INSERT INTO operator_audit VALUES (NEW.id); END; \
             ALTER TABLE messages ADD COLUMN future_metadata TEXT; \
             UPDATE messages SET future_metadata='future-schema-state' WHERE id=1; \
             COMMIT").expect("seed future durable schema");
        conn.execute_sync(
            "INSERT INTO operator_durable_state (id,payload,note) VALUES (1,?1,'opaque state')",
            &[SqlValue::Bytes(vec![0, 255, 17, 128, 0])],
        )
        .unwrap();
        conn.close_sync().unwrap();
        self.database_hash = archive_file_hash(&self.database);
    }

    fn assert_source_unchanged(&self) {
        assert_eq!(archive_file_hash(&self.database), self.database_hash);
        for (relative, expected) in &self.storage_hashes {
            assert_eq!(
                archive_file_hash(&self.storage.join(relative)),
                *expected,
                "source/recovery evidence changed: {}",
                relative.display()
            );
        }
    }
}

fn archive_cli_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_am"))
}

fn assert_archive_command_ok(outcome: &CommandOutcome) {
    assert!(
        outcome.exit_code == Some(0) && !outcome.timed_out,
        "archive command failed: {outcome:?}"
    );
}

fn run_archive_save(
    fixture: &ArchiveFixture,
    binary: &Path,
    flags: &[&str],
    timeout: Duration,
) -> (PathBuf, CommandOutcome) {
    let before = fixture.archives();
    let mut command = fixture.command(binary);
    command.args(["archive", "save"]).args(flags);
    let outcome = run_with_timeout(&mut command, timeout);
    assert_archive_command_ok(&outcome);
    let after = fixture.archives();
    let added: Vec<_> = after.difference(&before).cloned().collect();
    assert_eq!(
        added.len(),
        1,
        "save must publish exactly one completed ZIP"
    );
    (added[0].clone(), outcome)
}

fn historical_archive_artifact(relative: &Path) -> bool {
    relative.starts_with("doctor/forensics") || relative.starts_with("doctor/reclaimable")
}

fn assert_archive_storage(
    fixture: &ArchiveFixture,
    archive_path: &Path,
    exclude_recovery: bool,
) -> serde_json::Value {
    let mut archive = zip::ZipArchive::new(fs::File::open(archive_path).unwrap()).unwrap();
    let names: BTreeSet<_> = archive.file_names().map(str::to_string).collect();
    assert!(names.contains(ARCHIVE_SNAPSHOT_ENTRY));
    for (relative, expected) in &fixture.storage_hashes {
        let name = format!(
            "storage_repo/{}",
            relative.to_string_lossy().replace('\\', "/")
        );
        if exclude_recovery && historical_archive_artifact(relative) {
            assert!(!names.contains(&name), "historical copy included: {name}");
            continue;
        }
        let member = archive
            .by_name(&name)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            archive_reader_hash(member),
            *expected,
            "archive bytes: {name}"
        );
    }
    let mut metadata = String::new();
    archive
        .by_name("metadata.json")
        .unwrap()
        .read_to_string(&mut metadata)
        .unwrap();
    serde_json::from_str(&metadata).expect("archive metadata")
}

fn archive_timestamp_micros(value: &str) -> i64 {
    value
        .parse()
        .ok()
        .or_else(|| mcp_agent_mail_core::timestamps::iso_to_micros(value))
        .unwrap_or_else(|| panic!("invalid restored timestamp: {value}"))
}

fn assert_archive_rows(database: &Path, expected_messages: usize, durable_extension: bool) {
    let conn = mcp_agent_mail_db::pool::open_guarded_read_only_sqlite_file(
        database,
        "synthetic archive restore verification",
    )
    .expect("open restored snapshot read-only");
    let messages = conn.query_sync(
        "SELECT id,subject,body_md,attachments,recipients_json,archive_metadata_json,ack_required \
         FROM messages ORDER BY id", &[],
    ).unwrap();
    assert_eq!(messages.len(), expected_messages);
    for (ordinal, row) in messages.iter().enumerate() {
        let id = i64::try_from(ordinal).unwrap() * 3 + 1;
        assert_eq!(row.get_named::<i64>("id").unwrap(), id);
        assert_eq!(
            row.get_named::<String>("subject").unwrap(),
            format!("Synthetic message {id}")
        );
        assert_eq!(
            row.get_named::<String>("body_md").unwrap(),
            archive_fixture_body(id)
        );
        assert_eq!(
            row.get_named::<String>("attachments").unwrap(),
            archive_fixture_attachments()
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &row.get_named::<String>("recipients_json").unwrap()
            )
            .unwrap(),
            serde_json::from_str::<serde_json::Value>(&archive_fixture_recipients()).unwrap()
        );
        assert_eq!(
            row.get_named::<String>("archive_metadata_json").unwrap(),
            r#"{"recovery":"preserve"}"#
        );
        assert_eq!(row.get_named::<i64>("ack_required").unwrap(), 1);
    }
    let recipients = conn
        .query_sync(
            "SELECT message_id,agent_id,kind,CAST(read_ts AS TEXT) AS read_value, \
         CAST(ack_ts AS TEXT) AS ack_value \
         FROM message_recipients ORDER BY message_id,agent_id",
            &[],
        )
        .unwrap();
    assert_eq!(recipients.len(), expected_messages * 3);
    for (ordinal, recipient_group) in recipients.as_chunks::<3>().0.iter().enumerate() {
        let id = i64::try_from(ordinal).unwrap() * 3 + 1;
        for (row, (agent_id, kind)) in
            recipient_group
                .iter()
                .zip([(2_i64, "to"), (3, "cc"), (4, "bcc")])
        {
            assert_eq!(row.get_named::<i64>("message_id").unwrap(), id);
            assert_eq!(row.get_named::<i64>("agent_id").unwrap(), agent_id);
            assert_eq!(row.get_named::<String>("kind").unwrap(), kind);
            assert_eq!(
                archive_timestamp_micros(&row.get_named::<String>("read_value").unwrap()),
                ARCHIVE_FIXTURE_TS + id + 1
            );
            if agent_id == 2 {
                assert_eq!(
                    archive_timestamp_micros(&row.get_named::<String>("ack_value").unwrap()),
                    ARCHIVE_FIXTURE_TS + id + 2
                );
            } else {
                assert_eq!(row.get_by_name("ack_value"), Some(&SqlValue::Null));
            }
        }
    }
    let agents = conn
        .query_sync("SELECT id,registration_token FROM agents ORDER BY id", &[])
        .unwrap();
    assert_eq!(agents.len(), 4);
    for (ordinal, row) in agents.iter().enumerate() {
        let id = i64::try_from(ordinal + 1).unwrap();
        assert_eq!(row.get_named::<i64>("id").unwrap(), id);
        assert_eq!(
            row.get_named::<String>("registration_token").unwrap(),
            format!("{ARCHIVE_FIXTURE_TOKEN}-{id}")
        );
    }
    if durable_extension {
        let state = conn
            .query_sync(
                "SELECT payload,note FROM operator_durable_state WHERE id=1",
                &[],
            )
            .unwrap();
        assert_eq!(state.len(), 1);
        assert_eq!(
            state[0].get_by_name("payload"),
            Some(&SqlValue::Bytes(vec![0, 255, 17, 128, 0]))
        );
        assert_eq!(
            state[0].get_named::<String>("note").unwrap(),
            "opaque state"
        );
        let identity = conn
            .query_sync(
                "SELECT generation_id FROM db_identity WHERE singleton=0",
                &[],
            )
            .unwrap();
        assert_eq!(identity.len(), 1);
        assert_eq!(
            identity[0].get_named::<String>("generation_id").unwrap(),
            "archive-fixture-generation"
        );
        let future = conn
            .query_sync("SELECT future_metadata FROM messages WHERE id=1", &[])
            .unwrap();
        assert_eq!(
            future[0].get_named::<String>("future_metadata").unwrap(),
            "future-schema-state"
        );
        let schema = conn
            .query_sync(
                "SELECT name FROM sqlite_master WHERE name IN \
             ('operator_durable_note','operator_durable_view','operator_durable_audit')",
                &[],
            )
            .unwrap();
        assert_eq!(
            schema.len(),
            3,
            "custom index, view and trigger survive restoration"
        );
    }
}

fn restore_archive_fixture(
    fixture: &ArchiveFixture,
    archive: &Path,
    restore_root: &Path,
) -> CommandOutcome {
    fs::create_dir_all(restore_root).unwrap();
    let mut command = fixture.command(&archive_cli_binary());
    command
        .args(["archive", "restore"])
        .arg(archive)
        .arg("--force")
        .env(
            "DATABASE_URL",
            format!(
                "sqlite:///{}",
                restore_root.join("mailbox.sqlite3").display()
            ),
        )
        .env("STORAGE_ROOT", restore_root.join("storage_repo"));
    run_with_timeout(&mut command, Duration::from_secs(180))
}

#[test]
fn archive_large_mailbox_policy_roundtrip_preserves_private_state_and_evidence() {
    // Crosses the SQL row-copy page size and contains more than 6,000 real
    // archive files. The ignored benchmark scales this identical CLI path.
    let mut fixture = ArchiveFixture::new(2_001, 4);
    fixture.add_durable_extension();
    let binary = archive_cli_binary();
    let (full_archive, full_outcome) = run_archive_save(
        &fixture,
        &binary,
        &["--diagnostics", "--progress", "text"],
        Duration::from_secs(180),
    );
    let full_hash = archive_file_hash(&full_archive);
    let full_metadata = assert_archive_storage(&fixture, &full_archive, false);
    assert_eq!(full_metadata["scrub_preset"], "archive");
    assert_eq!(
        full_metadata["storage"]["recovery_artifacts_policy"],
        "include"
    );
    let records = archive_diagnostics(&full_outcome.stderr);
    assert!(!records.is_empty());
    assert!(records.iter().any(|record| record["status"] == "started"));
    assert!(records.iter().any(|record| record["status"] == "completed"));
    assert!(
        full_outcome.stderr.lines().any(|line| {
            line.starts_with("archive_save phase=snapshot_full_copy ") && line.contains("state=")
        }),
        "the unscoped private archive must report its native full-copy phase"
    );
    assert!(!full_outcome.stdout.contains(ARCHIVE_DIAGNOSTIC_PREFIX));
    assert!(!full_outcome.stderr.contains(ARCHIVE_PRIVATE_MARKER));
    assert!(!full_outcome.stderr.contains(ARCHIVE_FIXTURE_TOKEN));

    let (lean_archive, _) = run_archive_save(
        &fixture,
        &binary,
        &["--exclude-recovery-artifacts", "--compression-level", "1"],
        Duration::from_secs(180),
    );
    let lean_metadata = assert_archive_storage(&fixture, &lean_archive, true);
    assert_eq!(
        lean_metadata["storage"]["recovery_artifacts_policy"],
        "exclude"
    );
    let excluded = lean_metadata["storage"]["excluded_paths"]
        .as_array()
        .expect("omissions recorded");
    for relative in ["doctor/forensics", "doctor/reclaimable"] {
        assert!(
            excluded
                .iter()
                .any(|value| value.as_str() == Some(relative))
        );
    }
    assert_eq!(
        archive_file_hash(&full_archive),
        full_hash,
        "previous archive preserved"
    );
    fixture.assert_source_unchanged();
    let restored = fixture.root.path().join("restored");
    assert_archive_command_ok(&restore_archive_fixture(&fixture, &lean_archive, &restored));
    assert_archive_rows(&restored.join("mailbox.sqlite3"), fixture.messages, true);
    for (relative, expected) in &fixture.storage_hashes {
        let actual = restored.join("storage_repo").join(relative);
        if historical_archive_artifact(relative) {
            assert!(
                !actual.exists(),
                "excluded artifact restored: {}",
                relative.display()
            );
        } else {
            assert_eq!(
                archive_file_hash(&actual),
                *expected,
                "restored bytes: {}",
                relative.display()
            );
        }
    }
    fixture.assert_source_unchanged();
}

#[test]
fn archive_dry_run_does_not_open_sqlite_or_create_a_backup() {
    let fixture = ArchiveFixture::new(3, 1);
    let invalid_database = fixture.root.path().join("not-sqlite.db");
    fs::write(&invalid_database, b"synthetic non-SQLite data").unwrap();
    let invalid_hash = archive_file_hash(&invalid_database);
    let mut command = fixture.command(&archive_cli_binary());
    command
        .args([
            "archive",
            "save",
            "--dry-run",
            "--exclude-recovery-artifacts",
        ])
        .env(
            "DATABASE_URL",
            format!("sqlite:///{}", invalid_database.display()),
        );
    let outcome = run_with_timeout(&mut command, Duration::from_secs(30));
    assert_archive_command_ok(&outcome);
    assert!(outcome.stdout.contains("excluded_files="));
    assert!(outcome.stdout.contains("excluded_bytes="));
    assert!(!outcome.stderr.contains("phase=snapshot"));
    assert!(fixture.archives().is_empty());
    assert_eq!(archive_file_hash(&invalid_database), invalid_hash);
    for entry in fs::read_dir(fixture.root.path()).unwrap() {
        let name = entry.unwrap().file_name();
        assert!(
            !name.to_string_lossy().starts_with("not-sqlite.db-"),
            "dry-run opened SQLite or created sidecars"
        );
    }
    fixture.assert_source_unchanged();
}

#[test]
fn archive_failed_save_and_restore_preserve_last_backup_and_allow_retry() {
    let fixture = ArchiveFixture::new(31, 0);
    let binary = archive_cli_binary();
    let (previous, _) = run_archive_save(&fixture, &binary, &[], Duration::from_secs(120));
    let previous_hash = archive_file_hash(&previous);
    let before_failure = fixture.archives();
    let mut failed_save = fixture.command(&binary);
    failed_save.args(["archive", "save", "--project", "no-such-synthetic-project"]);
    let failure = run_with_timeout(&mut failed_save, Duration::from_secs(120));
    assert!(
        !failure.timed_out && failure.exit_code != Some(0),
        "invalid project should fail: {failure:?}"
    );
    assert_eq!(fixture.archives(), before_failure);
    assert_eq!(archive_file_hash(&previous), previous_hash);
    fixture.assert_source_unchanged();
    // A selected-project snapshot exercises the retained SQL-copy path.
    let (retry, _) = run_archive_save(
        &fixture,
        &binary,
        &["--project", "scale-mailbox"],
        Duration::from_secs(120),
    );
    assert_ne!(previous, retry);
    assert_eq!(archive_file_hash(&previous), previous_hash);
    let restored = fixture.root.path().join("restore-retry");
    assert_archive_command_ok(&restore_archive_fixture(&fixture, &previous, &restored));
    let restored_hash = archive_file_hash(&restored.join("mailbox.sqlite3"));
    let current_state = restored.join("storage_repo/operator-current-state.txt");
    fs::write(
        &current_state,
        b"preserve current mailbox during failed restore",
    )
    .unwrap();
    let truncated = fixture.root.path().join("interrupted-copy.zip");
    let source = fs::File::open(&retry).unwrap();
    let bytes_to_copy = source.metadata().unwrap().len() / 2;
    std::io::copy(
        &mut source.take(bytes_to_copy),
        &mut fs::File::create(&truncated).unwrap(),
    )
    .unwrap();
    let failure = restore_archive_fixture(&fixture, &truncated, &restored);
    assert!(
        !failure.timed_out && failure.exit_code != Some(0),
        "truncated ZIP should fail: {failure:?}"
    );
    assert_eq!(
        archive_file_hash(&restored.join("mailbox.sqlite3")),
        restored_hash
    );
    assert_eq!(
        fs::read(&current_state).unwrap(),
        b"preserve current mailbox during failed restore"
    );
    assert_archive_command_ok(&restore_archive_fixture(&fixture, &retry, &restored));
    assert_archive_rows(&restored.join("mailbox.sqlite3"), fixture.messages, false);
    let preserved_storage = fs::read_dir(&restored)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("storage_repo.backup-")
                && fs::read(entry.path().join("operator-current-state.txt"))
                    .is_ok_and(|bytes| bytes == b"preserve current mailbox during failed restore")
        });
    assert!(
        preserved_storage,
        "successful retry must retain the replaced mailbox storage"
    );
    assert_eq!(archive_file_hash(&previous), previous_hash);
    fixture.assert_source_unchanged();
}

#[test]
fn archive_exclusion_refuses_attachment_references_into_recovery_copies() {
    let mut fixture = ArchiveFixture::new(3, 0);
    let binary = archive_cli_binary();
    let (previous, _) = run_archive_save(&fixture, &binary, &[], Duration::from_secs(120));
    let previous_hash = archive_file_hash(&previous);
    for (field, reference) in [
        (
            "path",
            "projects/../doctor/forensics/old-run/storage.sqlite3".to_string(),
        ),
        (
            "original_path",
            fixture
                .storage
                .join("doctor/reclaimable/old-run/storage.sqlite3")
                .display()
                .to_string(),
        ),
    ] {
        let conn =
            mcp_agent_mail_db::DbConn::open_file(fixture.database.to_str().unwrap()).unwrap();
        let mut attachment = serde_json::Map::new();
        attachment.insert(field.to_string(), serde_json::Value::String(reference));
        conn.execute_sync(
            "UPDATE messages SET attachments=?1 WHERE id=1",
            &[SqlValue::Text(
                serde_json::Value::Array(vec![serde_json::Value::Object(attachment)]).to_string(),
            )],
        )
        .unwrap();
        conn.close_sync().unwrap();
        fixture.database_hash = archive_file_hash(&fixture.database);
        let before = fixture.archives();
        let mut command = fixture.command(&binary);
        command.args(["archive", "save", "--exclude-recovery-artifacts"]);
        let outcome = run_with_timeout(&mut command, Duration::from_secs(120));
        assert!(
            !outcome.timed_out && outcome.exit_code != Some(0),
            "referenced recovery artifact must prevent exclusion: {outcome:?}"
        );
        assert_eq!(fixture.archives(), before);
        assert_eq!(archive_file_hash(&previous), previous_hash);
        fixture.assert_source_unchanged();
    }
}

/// Reproducible real-CLI benchmark. Point AM_ARCHIVE_BASELINE_BIN at a previous
/// build to compare identical invocations against exactly the same mailbox.
/// Counts, raw diagnostics, elapsed times and verified coverage are retained
/// under AM_SHARE_ARCHIVE_ARTIFACT_DIR (or tests/artifacts/cli/archive_scale).
#[test]
#[ignore = "explicit large-mailbox CPU/disk benchmark; configure and run separately"]
fn archive_scale_benchmark() {
    fn setting(name: &str, default: usize) -> usize {
        std::env::var(name).map_or(default, |value| {
            value
                .parse()
                .unwrap_or_else(|error| panic!("invalid {name}: {error}"))
        })
    }
    let messages = setting("AM_ARCHIVE_SCALE_MESSAGES", 32_000);
    let recovery_mib = setting("AM_ARCHIVE_SCALE_RECOVERY_MIB", 256);
    let runs = setting("AM_ARCHIVE_SCALE_RUNS", 1);
    let timeout_seconds = setting("AM_ARCHIVE_SCALE_TIMEOUT_SECS", 900);
    assert!(messages > 0);
    assert!((1..=10).contains(&runs));
    let timeout = Duration::from_secs(u64::try_from(timeout_seconds).unwrap());
    let fixture = ArchiveFixture::new(messages, recovery_mib);
    let evidence = harness_artifacts_root("archive_scale").join(format!(
        "{}_{}",
        Utc::now().format("%Y%m%dT%H%M%S%.fZ"),
        std::process::id()
    ));
    fs::create_dir_all(&evidence).unwrap();
    eprintln!("archive scale evidence: {}", evidence.display());
    let mut binaries = Vec::new();
    if let Some(path) = std::env::var_os("AM_ARCHIVE_BASELINE_BIN") {
        binaries.push((
            "baseline",
            PathBuf::from(path).canonicalize().expect("baseline binary"),
        ));
    }
    binaries.push(("candidate", archive_cli_binary()));
    let mut samples = Vec::new();
    for iteration in 0..runs {
        let mut order = binaries.clone();
        // Alternate order when repeated, avoiding a fixed first-reader bias.
        if iteration % 2 == 1 {
            order.reverse();
        }
        for (kind, binary) in order {
            let before = fixture.archives();
            let mut command = fixture.command(&binary);
            command.args(["archive", "save", "--label", "scale-benchmark"]);
            let outcome = run_with_timeout(&mut command, timeout);
            let stem = format!("{iteration}-{kind}");
            fs::write(evidence.join(format!("{stem}.stdout.log")), &outcome.stdout).unwrap();
            fs::write(evidence.join(format!("{stem}.stderr.log")), &outcome.stderr).unwrap();
            let after = fixture.archives();
            let new_archives: Vec<_> = after.difference(&before).cloned().collect();
            let mut sample = serde_json::json!({
                "kind":kind, "binary":binary.display().to_string(), "iteration":iteration,
                "elapsed_ms":outcome.duration_ms, "exit_code":outcome.exit_code,
                "timed_out":outcome.timed_out, "published_archives":new_archives.len(),
                "completeness_verified":false,
            });
            if outcome.exit_code == Some(0) && !outcome.timed_out && new_archives.len() == 1 {
                let archive_path = &new_archives[0];
                assert_archive_storage(&fixture, archive_path, false);
                let mut archive =
                    zip::ZipArchive::new(fs::File::open(archive_path).unwrap()).unwrap();
                let mut uncompressed_bytes = 0_u64;
                for index in 0..archive.len() {
                    uncompressed_bytes += archive.by_index(index).unwrap().size();
                }
                sample["archive_bytes"] = fs::metadata(archive_path).unwrap().len().into();
                sample["uncompressed_bytes"] = uncompressed_bytes.into();
                sample["entries"] = archive.len().into();
                let snapshot_path = evidence.join(format!("{stem}.sqlite3"));
                std::io::copy(
                    &mut archive.by_name(ARCHIVE_SNAPSHOT_ENTRY).unwrap(),
                    &mut fs::File::create(&snapshot_path).unwrap(),
                )
                .unwrap();
                assert_archive_rows(&snapshot_path, fixture.messages, false);
                sample["completeness_verified"] = true.into();
            }
            samples.push(sample);
            fs::write(
                evidence.join("measurements.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "schema_version":1,
                    "fixture":{"messages":messages,"recipients":messages*3,
                        "storage_files":fixture.storage_hashes.len(),"recovery_mib":recovery_mib,
                        "source_database_sha256":fixture.database_hash},
                    "same_invocation_baseline":binaries.len()==2,
                    "samples":samples,
                }))
                .unwrap(),
            )
            .unwrap();
            fixture.assert_source_unchanged();
            assert_archive_command_ok(&outcome);
            assert_eq!(new_archives.len(), 1);
        }
    }
    // This is a separately labelled policy experiment, not an equal-coverage
    // speed comparison: it deliberately omits historical recovery material.
    let (excluded_archive, excluded_outcome) = run_archive_save(
        &fixture,
        &archive_cli_binary(),
        &["--exclude-recovery-artifacts", "--diagnostics"],
        timeout,
    );
    let metadata = assert_archive_storage(&fixture, &excluded_archive, true);
    fs::write(
        evidence.join("excluded.stdout.log"),
        &excluded_outcome.stdout,
    )
    .unwrap();
    fs::write(
        evidence.join("excluded.stderr.log"),
        &excluded_outcome.stderr,
    )
    .unwrap();
    fs::write(
        evidence.join("excluded.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "elapsed_ms":excluded_outcome.duration_ms,
            "archive_bytes":fs::metadata(excluded_archive).unwrap().len(),
            "comparison_scope":"explicitly omits historical recovery copies",
            "metadata":metadata,
        }))
        .unwrap(),
    )
    .unwrap();
    fixture.assert_source_unchanged();
}

// GH#365: direct native archive diagnostics regressions. Unlike the optional
// shell adapter above, these always exercise the built CLI and never contact a
// running mailbox. Every configuration and data path belongs to the fixture.
const ARCHIVE_DIAGNOSTIC_PREFIX: &str = "archive_save_diagnostic ";
const PRIVATE_PROJECT: &str = "diagnostic-private-project-sentinel";
const PRIVATE_LABEL: &str = "diagnostic-private-label-sentinel";
const PRIVATE_SUBJECT: &str = "Diagnostic private subject sentinel";
const PRIVATE_BODY: &str = "Diagnostic private body sentinel";
const PRIVATE_TOKEN: &str = "diagnostic-private-registration-token-sentinel";
const PRIVATE_BEARER: &str = "diagnostic-private-http-bearer-sentinel";
const RECOVERY_EVIDENCE: &[u8] = b"retained recovery evidence sentinel\n";

struct ArchiveDiagnosticFixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    database: PathBuf,
    storage: PathBuf,
}

impl ArchiveDiagnosticFixture {
    fn new(seed_database: bool) -> Self {
        let temp = tempfile::tempdir().expect("private archive fixture");
        // Use physical paths: macOS /tmp may be a symlink, and the snapshot
        // code intentionally refuses symlink ancestry.
        let root = temp.path().canonicalize().unwrap();
        let database = root.join("mailbox.sqlite3");
        let storage = root.join("storage");
        for path in [
            storage.join("doctor"),
            root.join("home/.config"),
            root.join("home/.cache"),
            root.join("home/.local/share"),
            root.join("tmp"),
            root.join("empty-bin"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        // Bound archive-output discovery to this fixture, outside STORAGE_ROOT.
        fs::write(root.join("Cargo.toml"), b"[workspace]\n").unwrap();
        fs::write(storage.join("payload.bin"), vec![b'x'; 131_073]).unwrap();
        fs::write(
            storage.join("doctor/retained-evidence.sqlite3"),
            RECOVERY_EVIDENCE,
        )
        .unwrap();

        let fixture = Self {
            _temp: temp,
            root,
            database,
            storage,
        };
        if seed_database {
            fixture.seed_database();
        }
        fixture
    }

    fn seed_database(&self) {
        use mcp_agent_mail_db::sqlmodel::Value;

        let conn = mcp_agent_mail_db::DbConn::open_file(self.database.to_string_lossy().as_ref())
            .expect("open isolated runtime database");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("create mailbox schema");
        conn.execute_raw("BEGIN IMMEDIATE").unwrap();
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, ?, ?, 0)",
            &[
                Value::Text(PRIVATE_PROJECT.to_string()),
                Value::Text(self.root.join("private-project").display().to_string()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (2, 'excluded-project', ?, 0)",
            &[Value::Text(self.root.join("excluded-project").display().to_string())],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts, registration_token) \
             VALUES (1, 1, 'GreenCastle', 'test', 'test', 0, 0, ?)",
            &[Value::Text(PRIVATE_TOKEN.to_string())],
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) \
             VALUES (2, 2, 'PurpleBear', 'test', 'test', 0, 0)",
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts) VALUES (1, 1, 1, ?, ?, 0)",
            &[
                Value::Text(PRIVATE_SUBJECT.to_string()),
                Value::Text(PRIVATE_BODY.to_string()),
            ],
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts) \
             VALUES (2, 2, 2, 'Excluded subject', 'Excluded body', 0); \
             INSERT INTO message_recipients (message_id, agent_id) VALUES (1, 1), (2, 2); \
             COMMIT",
        )
        .unwrap();
        conn.close_sync()
            .expect("close fixture database before CLI");
    }

    fn command(&self) -> Command {
        let home = self.root.join("home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_am"));
        command
            .env_clear()
            .current_dir(&self.root)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("PATH", self.root.join("empty-bin"))
            .env("AM_INTERFACE_MODE", "cli")
            .env("STORAGE_ROOT", &self.storage)
            .env(
                "DATABASE_URL",
                format!("sqlite:///{}", self.database.display()),
            )
            .env("HTTP_HOST", "127.0.0.1")
            .env("HTTP_PORT", "1")
            .env("HTTP_BEARER_TOKEN", PRIVATE_BEARER)
            .env("TUI_ENABLED", "false")
            .env("NO_COLOR", "1")
            .env("TERM", "dumb");
        command
    }

    fn save(&self, preset: &str, project: &str, diagnostics: bool) -> CommandOutcome {
        let mut command = self.command();
        command.args([
            "archive",
            "save",
            "--project",
            project,
            "--scrub-preset",
            preset,
            "--label",
            PRIVATE_LABEL,
        ]);
        if diagnostics {
            command.arg("--diagnostics");
        }
        run_with_timeout(&mut command, Duration::from_secs(120))
    }

    fn archives(&self) -> Vec<PathBuf> {
        let archive_dir = self.root.join("archived_mailbox_states");
        if !archive_dir.exists() {
            return Vec::new();
        }
        fs::read_dir(archive_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "zip"))
            .collect()
    }

    fn assert_evidence_preserved(&self) {
        assert_eq!(
            fs::read(self.storage.join("doctor/retained-evidence.sqlite3")).unwrap(),
            RECOVERY_EVIDENCE
        );
    }

    fn assert_source_preserved(&self) {
        let conn = mcp_agent_mail_db::DbConn::open_file(self.database.to_string_lossy().as_ref())
            .expect("reopen isolated source");
        for table in ["projects", "agents", "messages"] {
            let rows = conn
                .query_sync(&format!("SELECT COUNT(*) AS count FROM {table}"), &[])
                .unwrap();
            assert_eq!(rows[0].get_named::<i64>("count").unwrap(), 2);
        }
        let rows = conn
            .query_sync("SELECT body_md FROM messages WHERE id = 1", &[])
            .unwrap();
        assert_eq!(
            rows[0].get_named::<String>("body_md").unwrap(),
            PRIVATE_BODY
        );
        let rows = conn
            .query_sync("SELECT registration_token FROM agents WHERE id = 1", &[])
            .unwrap();
        assert_eq!(
            rows[0].get_named::<String>("registration_token").unwrap(),
            PRIVATE_TOKEN
        );
        conn.close_sync().unwrap();
    }
}

fn require_archive_success(outcome: &CommandOutcome) {
    assert!(
        outcome.exit_code == Some(0) && !outcome.timed_out,
        "archive fixture command failed: {outcome:?}"
    );
}

fn diagnostic_counter(record: &BTreeMap<&str, &str>, field: &str) -> u64 {
    record[field]
        .parse()
        .unwrap_or_else(|error| panic!("non-numeric {field} in diagnostic {record:?}: {error}"))
}

fn archive_diagnostics(stderr: &str) -> Vec<BTreeMap<&str, &str>> {
    let numeric_fields = [
        "elapsed_ms",
        "phase_elapsed_ms",
        "storage_files",
        "storage_bytes",
        "storage_size_unknown",
        "zip_files",
        "zip_bytes",
        "snapshot_bytes",
        "archive_bytes",
        "heartbeat",
    ];
    let mut records = Vec::new();
    for line in stderr.lines() {
        let Some(fields) = line.strip_prefix(ARCHIVE_DIAGNOSTIC_PREFIX) else {
            continue;
        };
        let mut record = BTreeMap::new();
        for field in fields.split_whitespace() {
            let (name, value) = field.split_once('=').expect("diagnostic key=value field");
            assert!(record.insert(name, value).is_none(), "duplicate {name}");
        }
        // The exact allowlist is also a privacy regression: no arbitrary path,
        // mailbox metadata, error message, or credential fields may slip in.
        assert_eq!(record.len(), numeric_fields.len() + 2, "{record:?}");
        assert!(matches!(
            record["phase"],
            "prepare"
                | "snapshot_source"
                | "snapshot"
                | "filter"
                | "scrub"
                | "finalize"
                | "archive_metadata"
                | "storage_enumeration"
                | "zip_writing"
                | "archive_verification"
                | "publication"
        ));
        assert!(matches!(
            record["status"],
            "started" | "completed" | "running" | "failed"
        ));
        for field in numeric_fields {
            diagnostic_counter(&record, field);
        }
        assert!(diagnostic_counter(&record, "heartbeat") <= 1);
        assert!(
            diagnostic_counter(&record, "phase_elapsed_ms")
                <= diagnostic_counter(&record, "elapsed_ms")
        );
        records.push(record);
    }
    for pair in records.windows(2) {
        for field in [
            "elapsed_ms",
            "storage_files",
            "storage_bytes",
            "storage_size_unknown",
            "zip_files",
            "zip_bytes",
        ] {
            assert!(
                diagnostic_counter(&pair[0], field) <= diagnostic_counter(&pair[1], field),
                "{field} moved backwards: {pair:?}"
            );
        }
    }
    records
}

fn assert_private_diagnostics(fixture: &ArchiveDiagnosticFixture, outcome: &CommandOutcome) {
    assert!(!outcome.stdout.contains(ARCHIVE_DIAGNOSTIC_PREFIX));
    let diagnostic_lines = outcome
        .stderr
        .lines()
        .filter(|line| line.starts_with(ARCHIVE_DIAGNOSTIC_PREFIX))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !diagnostic_lines.is_empty(),
        "missing diagnostics: {outcome:?}"
    );
    for secret in [
        fixture.root.to_str().unwrap(),
        PRIVATE_PROJECT,
        PRIVATE_LABEL,
        PRIVATE_SUBJECT,
        PRIVATE_BODY,
        PRIVATE_TOKEN,
        PRIVATE_BEARER,
        "retained-evidence.sqlite3",
    ] {
        assert!(
            !diagnostic_lines.contains(secret),
            "private fixture value in diagnostics"
        );
    }
}

#[test]
fn archive_save_diagnostics_cli_reports_phases_counters_and_preserves_scrub_semantics() {
    for preset in ["archive", "strict"] {
        let fixture = ArchiveDiagnosticFixture::new(true);
        let outcome = fixture.save(preset, PRIVATE_PROJECT, true);
        require_archive_success(&outcome);
        assert_private_diagnostics(&fixture, &outcome);
        let records = archive_diagnostics(&outcome.stderr);
        let phases = records
            .iter()
            .filter(|record| record["status"] == "started")
            .map(|record| record["phase"])
            .collect::<Vec<_>>();
        assert_eq!(
            phases,
            [
                "prepare",
                "snapshot_source",
                "snapshot",
                "filter",
                "scrub",
                "finalize",
                "archive_metadata",
                "storage_enumeration",
                "zip_writing",
                "archive_verification",
                "publication"
            ]
        );
        let completed = records
            .iter()
            .filter(|record| record["status"] == "completed")
            .map(|record| record["phase"])
            .collect::<Vec<_>>();
        assert_eq!(completed, phases);
        assert!(records.iter().all(|record| record["status"] != "failed"));
        let final_record = records.last().expect("publication diagnostic");
        assert_eq!(final_record["phase"], "publication");
        assert_eq!(final_record["status"], "completed");

        let archives = fixture.archives();
        assert_eq!(archives.len(), 1);
        let archive_path = &archives[0];
        let mut zip = zip::ZipArchive::new(fs::File::open(archive_path).unwrap()).unwrap();
        let mut storage_files = 0;
        let mut storage_bytes = 0;
        let mut zip_bytes = 0;
        for index in 0..zip.len() {
            let entry = zip.by_index(index).unwrap();
            assert_eq!(entry.compression(), zip::CompressionMethod::Deflated);
            zip_bytes += entry.size();
            if entry.name().unwrap().starts_with("storage_repo/") {
                storage_files += 1;
                storage_bytes += entry.size();
            }
        }
        assert_eq!(
            diagnostic_counter(final_record, "zip_files"),
            zip.len() as u64
        );
        assert_eq!(diagnostic_counter(final_record, "zip_bytes"), zip_bytes);
        assert_eq!(
            diagnostic_counter(final_record, "storage_files"),
            storage_files
        );
        assert_eq!(
            diagnostic_counter(final_record, "storage_bytes"),
            storage_bytes
        );
        assert_eq!(diagnostic_counter(final_record, "storage_size_unknown"), 0);
        assert_eq!(
            diagnostic_counter(final_record, "archive_bytes"),
            fs::metadata(archive_path).unwrap().len()
        );
        let mut evidence = Vec::new();
        zip.by_name("storage_repo/doctor/retained-evidence.sqlite3")
            .unwrap()
            .read_to_end(&mut evidence)
            .unwrap();
        assert_eq!(evidence, RECOVERY_EVIDENCE);
        let mut payload = Vec::new();
        zip.by_name("storage_repo/payload.bin")
            .unwrap()
            .read_to_end(&mut payload)
            .unwrap();
        assert_eq!(
            payload,
            fs::read(fixture.storage.join("payload.bin")).unwrap()
        );
        let metadata: serde_json::Value =
            serde_json::from_reader(zip.by_name("metadata.json").unwrap()).unwrap();
        assert_eq!(metadata["version"], 1);
        assert_eq!(metadata["scrub_preset"], preset);
        assert_eq!(metadata["label"], PRIVATE_LABEL);
        assert_eq!(metadata["projects_included"].as_array().unwrap().len(), 1);

        let mut snapshot = Vec::new();
        zip.by_name("snapshot/mailbox.sqlite3")
            .unwrap()
            .read_to_end(&mut snapshot)
            .unwrap();
        assert_eq!(
            diagnostic_counter(final_record, "snapshot_bytes"),
            snapshot.len() as u64
        );
        let snapshot_path = fixture.root.join("inspected-snapshot.sqlite3");
        fs::write(&snapshot_path, snapshot).unwrap();
        let snapshot_db =
            mcp_agent_mail_db::CanonicalDbConn::open_file(snapshot_path.to_string_lossy().as_ref())
                .expect("open private exported snapshot");
        let rows = snapshot_db
            .query_sync("SELECT slug FROM projects", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<String>("slug").unwrap(),
            PRIVATE_PROJECT
        );
        let rows = snapshot_db
            .query_sync("SELECT body_md FROM messages", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<String>("body_md").unwrap(),
            if preset == "archive" {
                PRIVATE_BODY
            } else {
                "[Message body redacted]"
            }
        );
        let token = snapshot_db
            .query_sync("SELECT registration_token FROM agents", &[])
            .unwrap()[0]
            .get_named::<Option<String>>("registration_token")
            .unwrap();
        assert_eq!(
            token.as_deref(),
            (preset == "archive").then_some(PRIVATE_TOKEN)
        );
        drop(snapshot_db);

        // The native restore planner accepts the saved format without stopping
        // services or taking mutation locks. Its existing target must survive.
        let restore_database = fixture.root.join("restore-target.sqlite3");
        let restore_storage = fixture.root.join("restore-storage");
        fs::write(&restore_database, b"existing restore target").unwrap();
        fs::create_dir(&restore_storage).unwrap();
        fs::write(restore_storage.join("existing.txt"), b"existing storage").unwrap();
        let mut restore = fixture.command();
        restore
            .env(
                "DATABASE_URL",
                format!("sqlite:///{}", restore_database.display()),
            )
            .env("STORAGE_ROOT", &restore_storage)
            .args([
                "archive",
                "restore",
                archive_path.to_str().unwrap(),
                "--dry-run",
            ]);
        let restored = run_with_timeout(&mut restore, Duration::from_secs(30));
        require_archive_success(&restored);
        assert!(restored.stdout.contains("Dry-run plan:"));
        assert_eq!(
            fs::read(restore_database).unwrap(),
            b"existing restore target"
        );
        assert_eq!(
            fs::read(restore_storage.join("existing.txt")).unwrap(),
            b"existing storage"
        );
        fixture.assert_evidence_preserved();
        fixture.assert_source_preserved();
    }
}

#[test]
fn archive_save_diagnostics_cli_is_opt_in() {
    let fixture = ArchiveDiagnosticFixture::new(true);
    let outcome = fixture.save("archive", PRIVATE_PROJECT, false);
    require_archive_success(&outcome);
    assert!(!outcome.stdout.contains(ARCHIVE_DIAGNOSTIC_PREFIX));
    assert!(!outcome.stderr.contains(ARCHIVE_DIAGNOSTIC_PREFIX));
    assert_eq!(fixture.archives().len(), 1);
    fixture.assert_evidence_preserved();
}

#[test]
fn archive_save_diagnostics_cli_attributes_failures_without_publishing() {
    for (seed_database, project, expected_phase) in [
        (false, PRIVATE_PROJECT, "snapshot_source"),
        (true, "missing-private-project-sentinel", "filter"),
    ] {
        let fixture = ArchiveDiagnosticFixture::new(seed_database);
        let outcome = fixture.save("archive", project, true);
        assert_eq!(outcome.exit_code, Some(1), "{outcome:?}");
        assert!(!outcome.timed_out, "{outcome:?}");
        assert_private_diagnostics(&fixture, &outcome);
        let records = archive_diagnostics(&outcome.stderr);
        let failures = records
            .iter()
            .filter(|record| record["status"] == "failed")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1, "{records:?}");
        assert_eq!(failures[0]["phase"], expected_phase);
        assert_eq!(records.last().unwrap()["status"], "failed");
        assert!(
            records
                .iter()
                .all(|record| record["phase"] != "publication")
        );
        assert!(fixture.archives().is_empty(), "failed save published a ZIP");
        fixture.assert_evidence_preserved();
        if seed_database {
            fixture.assert_source_preserved();
        }
    }
}
