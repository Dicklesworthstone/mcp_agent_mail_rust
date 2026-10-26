//! br-3ibsu: Native harness adapter for share/archive-heavy E2E suites.
//!
//! Current phase: managed adapter.
//! - Native runner entrypoint is Rust (`cargo test -p mcp-agent-mail-cli --test it share_archive_harness::`).
//! - Scenario execution delegates to shell suites (`share`, `share_verify_live`, `archive`).
//! - Harness captures deterministic metadata and copies legacy artifacts.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;

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
                "zip_writing",
                "storage_enumeration",
                "zip_writing",
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
            if entry.name().starts_with("storage_repo/") {
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
