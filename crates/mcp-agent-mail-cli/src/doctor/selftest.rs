//! Write-path and protocol self-tests for `am doctor`.
//!
//! These verbs prove *liveness of the write path* — something a green
//! `health_check` (which is largely read-derived) cannot guarantee. They are
//! the source of the `write_health` and `transport_health` verdicts that
//! Track C's decomposed `health_check` rolls up.
//!
//! ## `am doctor write-selftest` (br-bvq1x.3.2 / C2)
//!
//! Exercises the full write path — `ensure_project → register_agent (×2) →
//! send_message → acknowledge_message → file_reservation_paths →
//! release_file_reservations → list_agents` — inside an **isolated scratch
//! mailbox** (its own `STORAGE_ROOT` + SQLite file under a private tempdir),
//! and emits a per-dimension pass/fail verdict across `transport`, `schema`,
//! `lock`, `corruption`, and `permissions`. A failure isolates which
//! dimension broke, using the A1/A2 corruption taxonomy
//! ([`mcp_agent_mail_db::classify_db_error_message`]).
//!
//! ### Isolation, bounding, and cleanup (the three C2 invariants)
//!
//! The selftest must never touch the operator's real archive, must be
//! timeout-guarded, and must clean up after itself. We satisfy all three by
//! running the real sequence in a **child process** of the same binary:
//!
//! * **Isolation** — the child gets `STORAGE_ROOT`/`DATABASE_URL` pointed at a
//!   private tempdir, so its global [`mcp_agent_mail_core::Config`] and DB pool
//!   are completely separate from the parent's. The child additionally asserts
//!   (before any write) that the effective storage root resolves *inside* the
//!   scratch directory and refuses to run otherwise — defeating a stray
//!   `config.env`/`STORAGE_ROOT` that might otherwise redirect it at the live
//!   mailbox.
//! * **Bounding** — the parent waits with a hard deadline and `kill()`s the
//!   child on timeout. An in-process `block_on` against a hung/corrupt DB
//!   cannot be cancelled safely; a child process can.
//! * **Cleanup** — the scratch [`tempfile::TempDir`] is removed once the child
//!   is reaped.
//!
//! The same sequence body ([`run_selftest_sequence_in_process`]) is reused for
//! in-process tests so the real write path is covered without spawning.

#![forbid(unsafe_code)]

use crate::output::CliOutputFormat;
use crate::{CliError, CliResult};
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Sentinel: when set to `1` the `write-selftest` handler runs the *inner*
/// sequence against the (scratch) global config instead of orchestrating a
/// child. Set by the parent on the spawned child; never set by operators.
const INNER_ENV: &str = "AM_DOCTOR_WRITE_SELFTEST_INNER";
/// Absolute path of the scratch root the child must stay within.
const SCRATCH_ENV: &str = "AM_DOCTOR_WRITE_SELFTEST_SCRATCH";
/// Absolute path used as the scratch project `human_key`.
const PROJECT_ENV: &str = "AM_DOCTOR_WRITE_SELFTEST_PROJECT";
/// Operator override for the parent's hard timeout (seconds).
const TIMEOUT_ENV: &str = "AM_DOCTOR_WRITE_SELFTEST_TIMEOUT_SECS";
/// Default hard timeout for the child. Generous: a cold child pays for pool
/// warmup, schema migration, and git archive init before the first write.
const DEFAULT_TIMEOUT_SECS: u64 = 45;

/// The two scratch agents. Both are valid `adjective+noun` identities.
const SENDER: &str = "GreenCastle";
const RECIPIENT: &str = "BlueLake";

/// The five write-path dimensions C2 reports independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dimension {
    /// Can we reach/open the store at all (connect/config/FD).
    Transport,
    /// Are the required tables/columns present (schema shape).
    Schema,
    /// Could we take write locks without busy/contention/pool exhaustion.
    Lock,
    /// Did any corruption-class error surface (B-tree/WAL/FTS/FK).
    Corruption,
    /// Filesystem/permission/read-only-FS/host-pressure write blockers.
    Permissions,
}

impl Dimension {
    const ALL: [Self; 5] = [
        Self::Transport,
        Self::Schema,
        Self::Lock,
        Self::Corruption,
        Self::Permissions,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Schema => "schema",
            Self::Lock => "lock",
            Self::Corruption => "corruption",
            Self::Permissions => "permissions",
        }
    }
}

/// Map a failure message onto the dimension that broke, leaning on the shared
/// A1/A2 corruption taxonomy so this stays consistent with `am doctor` and the
/// `db_error_to_mcp_error` chokepoint. Permission/read-only signals win over
/// the DB classifier because they are unambiguous filesystem facts.
fn classify_dimension(err_msg: &str) -> Dimension {
    let lowered = err_msg.to_ascii_lowercase();
    if lowered.contains("permission denied")
        || lowered.contains("eacces")
        || lowered.contains("read-only file system")
        || lowered.contains("readonly database")
        || lowered.contains("read-only database")
    {
        return Dimension::Permissions;
    }

    use mcp_agent_mail_db::DbErrorClass as C;
    match mcp_agent_mail_db::classify_db_error_message(err_msg).class {
        C::SchemaDriftOrMissingTables => Dimension::Schema,
        C::BusyRetryable | C::PoolExhaustion | C::LiveOwnerNoActivityLock => Dimension::Lock,
        C::MainDbBtreeCorruption
        | C::WalSidecarCorruption
        | C::FtsIndexCorruption
        | C::ForeignKeyInconsistency
        | C::EngineProbeLimitation => Dimension::Corruption,
        C::HostPressure => Dimension::Permissions,
        C::FdExhaustion | C::ConnectionOrConfigError => Dimension::Transport,
    }
}

/// One executed step of the write-path sequence.
#[derive(Debug, Clone)]
struct StepResult {
    name: &'static str,
    ok: bool,
    skipped: bool,
    error: Option<String>,
    error_class: Option<String>,
    dimension: Option<Dimension>,
}

impl StepResult {
    const fn passed(name: &'static str) -> Self {
        Self {
            name,
            ok: true,
            skipped: false,
            error: None,
            error_class: None,
            dimension: None,
        }
    }

    const fn skipped(name: &'static str) -> Self {
        Self {
            name,
            ok: false,
            skipped: true,
            error: None,
            error_class: None,
            dimension: None,
        }
    }

    fn failed(name: &'static str, err_msg: String) -> Self {
        let classification = mcp_agent_mail_db::classify_db_error_message(&err_msg);
        let dimension = classify_dimension(&err_msg);
        Self {
            name,
            ok: false,
            skipped: false,
            error: Some(err_msg),
            error_class: Some(classification.class.as_str().to_string()),
            dimension: Some(dimension),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "ok": self.ok,
            "skipped": self.skipped,
            "error": self.error.clone(),
            "error_class": self.error_class.clone(),
            "dimension": self.dimension.map(Dimension::as_str),
        })
    }
}

/// Full report of an in-process write-path sequence run.
#[derive(Debug, Clone)]
struct WriteSelftestReport {
    ok: bool,
    steps: Vec<StepResult>,
    /// The first failing dimension, if any.
    failing_dimension: Option<Dimension>,
    /// Per-dimension verdict detail, keyed by dimension order in `Dimension::ALL`.
    failed_dimensions: Vec<(Dimension, String)>,
}

impl WriteSelftestReport {
    /// Build the per-dimension verdict block. Every dimension defaults to
    /// `pass`; the one implicated by the first failure flips to `fail`.
    fn dimensions_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        for dim in Dimension::ALL {
            let failed = self.failed_dimensions.iter().find(|(d, _)| *d == dim);
            let entry = match failed {
                Some((_, detail)) => json!({ "status": "fail", "detail": detail.clone() }),
                None => json!({ "status": "pass" }),
            };
            map.insert(dim.as_str().to_string(), entry);
        }
        Value::Object(map)
    }

    fn to_json(&self) -> Value {
        json!({
            "ok": self.ok,
            "failing_dimension": self.failing_dimension.map(Dimension::as_str),
            "dimensions": self.dimensions_json(),
            "steps": self.steps.iter().map(StepResult::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Entry point for `am doctor write-selftest`.
///
/// Dispatches to the inner sequence (when invoked as the spawned child) or to
/// the orchestrator (the operator-facing path).
pub fn handle_write_selftest(format: Option<CliOutputFormat>) -> CliResult<()> {
    if std::env::var(INNER_ENV).as_deref() == Ok("1") {
        run_inner()
    } else {
        orchestrate(format)
    }
}

/// Parent path for the write-path self-test.
fn orchestrate(format: Option<CliOutputFormat>) -> CliResult<()> {
    orchestrate_selftest(
        format,
        "write-selftest",
        INNER_ENV,
        "write_path",
        "write-path",
    )
}

/// Shared orchestrator: set up a scratch mailbox, spawn the `subcommand` child
/// in inner mode, bound it with a hard timeout, clean up, and wrap the child's
/// report in the stable doctor envelope. Exit 0 on pass, 1 on fail.
fn orchestrate_selftest(
    format: Option<CliOutputFormat>,
    subcommand: &str,
    inner_env: &str,
    selftest_label: &str,
    human_label: &str,
) -> CliResult<()> {
    let started = Instant::now();

    let td = tempfile::TempDir::new()
        .map_err(|e| CliError::Other(format!("could not create scratch tempdir: {e}")))?;
    let scratch = td.path().to_path_buf();
    let storage_root = scratch.join("storage");
    let project_dir = scratch.join("scratch_project");
    std::fs::create_dir_all(&storage_root)
        .map_err(|e| CliError::Other(format!("could not create scratch storage root: {e}")))?;
    std::fs::create_dir_all(&project_dir)
        .map_err(|e| CliError::Other(format!("could not create scratch project dir: {e}")))?;
    let db_path = scratch.join("storage.sqlite3");
    let database_url = format!("sqlite:///{}", db_path.display());

    let exe = std::env::current_exe()
        .map_err(|e| CliError::Other(format!("could not resolve current executable: {e}")))?;

    let timeout_secs = std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);

    let spawn = spawn_inner_child(
        &exe,
        subcommand,
        inner_env,
        &scratch,
        &storage_root,
        &project_dir,
        &database_url,
        Duration::from_secs(timeout_secs),
    );

    let duration_ms = started.elapsed().as_millis() as u64;

    let (inner_value, all_ok) = match spawn {
        ChildOutcome::Completed { stdout, stderr } => match extract_inner_json(&stdout) {
            Some(value) => {
                let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
                (value, ok)
            }
            None => (
                json!({
                    "ok": false,
                    "error": "child produced no parseable selftest JSON",
                    "raw_stdout_tail": tail(&stdout, 512),
                    "raw_stderr_tail": tail(&stderr, 512),
                }),
                false,
            ),
        },
        ChildOutcome::TimedOut { secs } => (
            json!({
                "ok": false,
                "timed_out": true,
                "error": format!("{human_label} selftest subprocess timed out after {secs}s"),
            }),
            false,
        ),
        ChildOutcome::SpawnError { message } => (
            json!({
                "ok": false,
                "error": format!("could not run {human_label} selftest subprocess: {message}"),
            }),
            false,
        ),
    };

    // The scratch dir is removed when `td` drops below.
    let envelope = json!({
        "schema_version": "1.0",
        "doctor_version": super::runs::DOCTOR_VERSION,
        "doctor_contract_version": super::runs::DOCTOR_CONTRACT_VERSION,
        "tool": "am",
        "tool_version": env!("CARGO_PKG_VERSION"),
        "selftest": selftest_label,
        "ok": all_ok,
        "scratch_storage_root": storage_root.to_string_lossy().into_owned(),
        "scratch_database_url": database_url.clone(),
        "timeout_secs": timeout_secs,
        "duration_ms": duration_ms,
        "result": inner_value,
    });

    print_envelope(&envelope, format)?;
    drop(td);

    if all_ok {
        Ok(())
    } else {
        eprintln!("error: doctor {human_label} selftest reported failures");
        Err(CliError::ExitCode(1))
    }
}

/// Result of running the inner child to completion / timeout.
enum ChildOutcome {
    Completed { stdout: String, stderr: String },
    TimedOut { secs: u64 },
    SpawnError { message: String },
}

/// Spawn the inner child for `subcommand` with a scratch environment and wait
/// with a hard deadline, draining its pipes on dedicated threads to avoid
/// pipe-buffer deadlock. `inner_env` is the sentinel that flips the child into
/// inner mode.
#[allow(clippy::too_many_arguments)]
fn spawn_inner_child(
    exe: &Path,
    subcommand: &str,
    inner_env: &str,
    scratch: &Path,
    storage_root: &Path,
    project_dir: &Path,
    database_url: &str,
    timeout: Duration,
) -> ChildOutcome {
    let mut child = match Command::new(exe)
        .arg("doctor")
        .arg(subcommand)
        .arg("--format")
        .arg("json")
        .env(inner_env, "1")
        .env(SCRATCH_ENV, scratch)
        .env(PROJECT_ENV, project_dir)
        .env("STORAGE_ROOT", storage_root)
        .env("DATABASE_URL", database_url)
        // The scratch project lives under a private tempdir, and STORAGE_ROOT is
        // an isolated tempdir too — so the ephemeral-project-root guard (which
        // exists to stop test tempdirs polluting the real archive) is exactly
        // the case it tells us to bypass for an intentional registration.
        .env("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1")
        .env("ALLOW_EPHEMERAL_PROJECTS_IN_DEFAULT_STORAGE", "true")
        .env("AM_INTERFACE_MODE", "cli")
        .env("TUI_ENABLED", "false")
        .env("LOG_LEVEL", "error")
        .env("RUST_LOG", "error")
        // Don't let the child re-read an operator timeout override.
        .env_remove(TIMEOUT_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return ChildOutcome::SpawnError {
                message: e.to_string(),
            };
        }
    };

    let stdout_handle = child.stdout.take().map(spawn_pipe_reader);
    let stderr_handle = child.stderr.take().map(spawn_pipe_reader);

    let deadline = Instant::now() + timeout;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_status)) => break false,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break true;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break false,
        }
    };

    let stdout = stdout_handle.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = stderr_handle.and_then(|h| h.join().ok()).unwrap_or_default();

    if timed_out {
        ChildOutcome::TimedOut {
            secs: timeout.as_secs(),
        }
    } else {
        ChildOutcome::Completed { stdout, stderr }
    }
}

fn spawn_pipe_reader<R: Read + Send + 'static>(
    mut reader: R,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = reader.read_to_string(&mut buf);
        buf
    })
}

/// Hard isolation gate shared by the inner self-tests: the effective storage
/// root MUST resolve inside the scratch dir. If a stray config redirected the
/// child at the live mailbox, refuse to run a single write. Returns the
/// validated scratch project key, or an error JSON the caller should print
/// before exiting 1.
fn scratch_project_key_or_guard_failure() -> Result<String, Value> {
    let project_key = std::env::var(PROJECT_ENV).unwrap_or_default();
    let scratch = std::env::var(SCRATCH_ENV).unwrap_or_default();
    let storage_root = mcp_agent_mail_core::Config::get().storage_root.clone();
    let isolation_ok = !scratch.is_empty()
        && path_is_within(&storage_root, Path::new(&scratch))
        && !project_key.is_empty();

    if isolation_ok {
        Ok(project_key)
    } else {
        Err(json!({
            "ok": false,
            "isolation_verified": false,
            "error": "scratch isolation guard failed; refusing to exercise the write path",
            "effective_storage_root": storage_root.to_string_lossy().into_owned(),
            "expected_scratch_root": scratch,
        }))
    }
}

/// Child path: verify isolation, then run the real write sequence and print its
/// JSON report.
fn run_inner() -> CliResult<()> {
    let project_key = match scratch_project_key_or_guard_failure() {
        Ok(key) => key,
        Err(value) => {
            println!("{value}");
            return Err(CliError::ExitCode(1));
        }
    };

    let report = run_selftest_sequence_in_process(&project_key);
    let mut value = report.to_json();
    if let Value::Object(ref mut map) = value {
        map.insert("isolation_verified".to_string(), Value::Bool(true));
    }
    println!("{value}");

    if report.ok {
        Ok(())
    } else {
        Err(CliError::ExitCode(1))
    }
}

/// The ordered step names of the write-path sequence.
const SEQUENCE: [&str; 8] = [
    "ensure_project",
    "register_agent_sender",
    "register_agent_recipient",
    "send_message",
    "acknowledge_message",
    "file_reservation_paths",
    "release_file_reservations",
    "list_agents",
];

/// Run the full write-path sequence against the *current process'* global
/// config + pool (which the caller must have pointed at an isolated scratch
/// mailbox). Reused by the inner child and by in-process tests.
///
/// The sequence short-circuits on the first failure; unreached steps are
/// recorded as `skipped` so the report always lists the full sequence.
#[allow(clippy::too_many_lines)]
fn run_selftest_sequence_in_process(project_key: &str) -> WriteSelftestReport {
    use fastmcp::prelude::McpContext;
    use mcp_agent_mail_tools::{identity, messaging, reservations};

    let rt = match asupersync::runtime::RuntimeBuilder::current_thread().build() {
        Ok(rt) => rt,
        Err(e) => {
            return finalize_report(vec![StepResult::failed(
                "runtime_init",
                format!("could not build async runtime: {e}"),
            )]);
        }
    };

    let project_key = project_key.to_string();
    rt.block_on(async move {
        let ctx = McpContext::new(asupersync::Cx::for_request(), 1);
        let mut steps: Vec<StepResult> = Vec::new();

        // 1: ensure_project
        match identity::ensure_project(&ctx, project_key.clone(), None).await {
            Ok(_) => steps.push(StepResult::passed("ensure_project")),
            Err(e) => {
                steps.push(StepResult::failed("ensure_project", e.message));
                return finalize_report(steps);
            }
        }

        // 2: register_agent (sender)
        match identity::register_agent(
            &ctx,
            project_key.clone(),
            "claude-code".to_string(),
            "selftest".to_string(),
            Some(SENDER.to_string()),
            Some("doctor write-selftest sender".to_string()),
            None,
            None,
            None,
        )
        .await
        {
            Ok(_) => steps.push(StepResult::passed("register_agent_sender")),
            Err(e) => {
                steps.push(StepResult::failed("register_agent_sender", e.message));
                return finalize_report(steps);
            }
        }

        // 3: register_agent (recipient)
        match identity::register_agent(
            &ctx,
            project_key.clone(),
            "claude-code".to_string(),
            "selftest".to_string(),
            Some(RECIPIENT.to_string()),
            Some("doctor write-selftest recipient".to_string()),
            None,
            None,
            None,
        )
        .await
        {
            Ok(_) => steps.push(StepResult::passed("register_agent_recipient")),
            Err(e) => {
                steps.push(StepResult::failed("register_agent_recipient", e.message));
                return finalize_report(steps);
            }
        }

        // 4: send_message (ack_required) — capture the new message id
        let send_payload = match messaging::send_message(
            &ctx,
            project_key.clone(),
            SENDER.to_string(),
            vec![RECIPIENT.to_string()],
            "doctor write-selftest".to_string(),
            "Synthetic write-path probe. Safe to ignore.".to_string(),
            None,
            None,
            None,
            None,
            Some("normal".to_string()),
            Some(true),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        {
            Ok(payload) => {
                steps.push(StepResult::passed("send_message"));
                payload
            }
            Err(e) => {
                steps.push(StepResult::failed("send_message", e.message));
                return finalize_report(steps);
            }
        };

        let message_id = match parse_message_id(&send_payload) {
            Some(id) => id,
            None => {
                steps.push(StepResult::failed(
                    "acknowledge_message",
                    "send_message returned no parseable message id".to_string(),
                ));
                return finalize_report(steps);
            }
        };

        // 5: acknowledge_message (read-modify-write as the recipient)
        match messaging::acknowledge_message(
            &ctx,
            project_key.clone(),
            RECIPIENT.to_string(),
            message_id,
        )
        .await
        {
            Ok(_) => steps.push(StepResult::passed("acknowledge_message")),
            Err(e) => {
                steps.push(StepResult::failed("acknowledge_message", e.message));
                return finalize_report(steps);
            }
        }

        // 6: file_reservation_paths
        match reservations::file_reservation_paths(
            &ctx,
            project_key.clone(),
            SENDER.to_string(),
            vec!["src/**".to_string()],
            Some(60),
            Some(true),
            Some("doctor write-selftest".to_string()),
        )
        .await
        {
            Ok(_) => steps.push(StepResult::passed("file_reservation_paths")),
            Err(e) => {
                steps.push(StepResult::failed("file_reservation_paths", e.message));
                return finalize_report(steps);
            }
        }

        // 7: release_file_reservations
        match reservations::release_file_reservations(
            &ctx,
            project_key.clone(),
            SENDER.to_string(),
            Some(vec!["src/**".to_string()]),
            None,
        )
        .await
        {
            Ok(_) => steps.push(StepResult::passed("release_file_reservations")),
            Err(e) => {
                steps.push(StepResult::failed("release_file_reservations", e.message));
                return finalize_report(steps);
            }
        }

        // 8: list_agents (read-after-write)
        match identity::list_agents(&ctx, project_key.clone()).await {
            Ok(_) => steps.push(StepResult::passed("list_agents")),
            Err(e) => {
                steps.push(StepResult::failed("list_agents", e.message));
                return finalize_report(steps);
            }
        }

        finalize_report(steps)
    })
}

/// Pad the executed steps with `skipped` entries for any unreached step and
/// derive the overall verdict + the first failing dimension.
fn finalize_report(mut steps: Vec<StepResult>) -> WriteSelftestReport {
    // Only pad when the steps recorded so far are a strict prefix of the
    // canonical sequence (the `runtime_init` failure path is not).
    if steps.len() < SEQUENCE.len()
        && steps
            .iter()
            .zip(SEQUENCE.iter())
            .all(|(s, name)| s.name == *name)
    {
        for name in SEQUENCE.iter().skip(steps.len()) {
            steps.push(StepResult::skipped(name));
        }
    }

    let first_failure = steps.iter().find(|s| !s.ok && !s.skipped);
    let failing_dimension = first_failure.and_then(|s| s.dimension);
    let failed_dimensions = first_failure
        .and_then(|s| {
            s.dimension
                .map(|d| (d, s.error.clone().unwrap_or_else(|| format!("step `{}` failed", s.name))))
        })
        .into_iter()
        .collect();
    let ok = first_failure.is_none() && steps.iter().any(|s| s.ok);

    WriteSelftestReport {
        ok,
        steps,
        failing_dimension,
        failed_dimensions,
    }
}

/// Parse the new message id out of a `send_message` success payload.
///
/// The canonical shape is
/// `{ "deliveries": [{ "project": ..., "payload": { "id": <id>, ... } }], ... }`;
/// the flat `id`/`message_id` fallbacks tolerate other surfaces.
fn parse_message_id(payload: &str) -> Option<i64> {
    let value: Value = serde_json::from_str(payload).ok()?;
    value
        .pointer("/deliveries/0/payload/id")
        .and_then(Value::as_i64)
        .or_else(|| value.get("id").and_then(Value::as_i64))
        .or_else(|| value.get("message_id").and_then(Value::as_i64))
}

/// True iff `candidate` is equal to or nested under `base` (lexical check on
/// the already-canonical config path; both are absolute).
fn path_is_within(candidate: &Path, base: &Path) -> bool {
    let candidate = candidate.canonicalize().unwrap_or_else(|_| candidate.to_path_buf());
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    candidate.starts_with(&base)
}

/// Pull the selftest JSON object out of child stdout, tolerating any leading
/// noise by scanning lines from the end for a parseable object.
fn extract_inner_json(stdout: &str) -> Option<Value> {
    let trimmed = stdout.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    for line in stdout.lines().rev() {
        let line = line.trim();
        if line.starts_with('{')
            && let Ok(value) = serde_json::from_str::<Value>(line)
        {
            return Some(value);
        }
    }
    None
}

fn tail(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        s.to_string()
    } else {
        chars[chars.len() - n..].iter().collect()
    }
}

fn print_envelope(envelope: &Value, _format: Option<CliOutputFormat>) -> CliResult<()> {
    // A contract surface: always JSON regardless of format request (TOON would
    // erase types; table is lossy), matching `handle_selftest`.
    let s = serde_json::to_string_pretty(envelope)
        .map_err(|e| CliError::Other(format!("serializing selftest envelope: {e}")))?;
    println!("{s}");
    Ok(())
}

// ============================================================================
// C3 — MCP decode self-test (br-bvq1x.3.3 / `am doctor mcp-selftest`)
// ============================================================================
//
// Proves the JSON-RPC decode + tool-dispatch path is healthy: a valid
// `initialize` decodes, a malformed frame is rejected as a PROTOCOL error
// (never silently accepted, never mislabeled as a database error), tool schema
// negotiation succeeds, and a harmless write + read-after-write round-trips
// THROUGH the MCP transport (decode → dispatch → tool). This is the ts2 anchor:
// MCP writes once failed with `rmcp JsonRpcMessage` deserialization errors
// BEFORE tool execution while reads still returned stale data, yet
// `health_check` stayed green. This produces the `transport_health` verdict for
// `health_check`.

/// Sentinel that flips the `mcp-selftest` handler into inner (child) mode.
const MCP_INNER_ENV: &str = "AM_DOCTOR_MCP_SELFTEST_INNER";

/// MCP protocol version this server negotiates (matches the conformance suite).
const PROTOCOL_VERSION: &str = "2024-11-05";

/// The L2 rmcp-decode fixture: a structurally invalid JSON-RPC frame. Decoding
/// it MUST fail (and be classed as a protocol error), mirroring
/// `protocol_compliance.rs::malformed_inputs_do_not_crash_the_server_loop`.
const L2_MALFORMED_FIXTURE: &[u8] = b"{not-json}\n";

/// Operator-facing guidance attached to a protocol-class failure — keeps the
/// failure firmly in the PROTOCOL lane, never "database error".
const PROTOCOL_GUIDANCE: &str = "MCP JSON-RPC decode failed before tool execution. This is a \
PROTOCOL/transport problem, not a database error. Check the client/server MCP protocolVersion \
negotiation (this server speaks 2024-11-05), the JSON-RPC framing (one JSON object per line for \
stdio; a correct Content-Length for HTTP), and that the client emits well-formed \
initialize/tools/call envelopes.";

/// One named check in the MCP self-test.
#[derive(Debug, Clone)]
struct CheckResult {
    name: &'static str,
    ok: bool,
    detail: Option<String>,
}

impl CheckResult {
    const fn pass(name: &'static str) -> Self {
        Self {
            name,
            ok: true,
            detail: None,
        }
    }

    const fn pass_with(name: &'static str, detail: String) -> Self {
        Self {
            name,
            ok: true,
            detail: Some(detail),
        }
    }

    const fn fail(name: &'static str, detail: String) -> Self {
        Self {
            name,
            ok: false,
            detail: Some(detail),
        }
    }

    fn to_json(&self) -> Value {
        json!({ "name": self.name, "ok": self.ok, "detail": self.detail.clone() })
    }
}

/// Full report of the MCP decode self-test.
#[derive(Debug, Clone)]
struct McpSelftestReport {
    ok: bool,
    /// True when any decode/protocol-layer check failed (drives the protocol
    /// classification + version guidance).
    protocol_class_failure: bool,
    negotiated_protocol_version: Option<String>,
    checks: Vec<CheckResult>,
}

impl McpSelftestReport {
    fn failure_class(&self) -> Option<&'static str> {
        if self.protocol_class_failure {
            Some("protocol")
        } else if !self.ok {
            Some("tool_dispatch")
        } else {
            None
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "ok": self.ok,
            "expected_protocol_version": PROTOCOL_VERSION,
            "negotiated_protocol_version": self.negotiated_protocol_version.clone(),
            "failure_class": self.failure_class(),
            "guidance": if self.protocol_class_failure { Some(PROTOCOL_GUIDANCE) } else { None },
            "checks": self.checks.iter().map(CheckResult::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Entry point for `am doctor mcp-selftest`.
pub fn handle_mcp_selftest(format: Option<CliOutputFormat>) -> CliResult<()> {
    if std::env::var(MCP_INNER_ENV).as_deref() == Ok("1") {
        run_mcp_inner()
    } else {
        orchestrate_selftest(
            format,
            "mcp-selftest",
            MCP_INNER_ENV,
            "mcp_decode",
            "mcp-decode",
        )
    }
}

/// Child path: verify isolation, run the MCP round-trip, print the report.
fn run_mcp_inner() -> CliResult<()> {
    let project_key = match scratch_project_key_or_guard_failure() {
        Ok(key) => key,
        Err(value) => {
            println!("{value}");
            return Err(CliError::ExitCode(1));
        }
    };

    let report = run_mcp_selftest_in_process(&project_key);
    let mut value = report.to_json();
    if let Value::Object(ref mut map) = value {
        map.insert("isolation_verified".to_string(), Value::Bool(true));
    }
    println!("{value}");

    if report.ok {
        Ok(())
    } else {
        Err(CliError::ExitCode(1))
    }
}

/// A `Write` sink shared with a server thread so the parent can snapshot the
/// transport output after the round-trip completes.
#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl SharedBuf {
    fn snapshot(&self) -> Vec<u8> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Build the `initialize` request used for decode + negotiation.
fn build_initialize_request(id: i64) -> fastmcp::JsonRpcRequest {
    fastmcp::JsonRpcRequest::new(
        "initialize",
        Some(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "am-doctor-mcp-selftest",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })),
        id,
    )
}

/// Build a `tools/call` request.
fn tools_call_request(name: &str, arguments: Value, id: i64) -> fastmcp::JsonRpcRequest {
    fastmcp::JsonRpcRequest::new(
        "tools/call",
        Some(json!({ "name": name, "arguments": arguments })),
        id,
    )
}

/// Run the MCP decode self-test against the (scratch) global config + pool.
/// Reused by the inner child and by in-process tests.
fn run_mcp_selftest_in_process(project_key: &str) -> McpSelftestReport {
    use fastmcp::{Cx, JsonRpcMessage, StdioTransport, Transport};
    use std::io::Cursor;

    let mut checks: Vec<CheckResult> = Vec::new();
    let mut protocol_class_failure = false;
    let mut negotiated_version: Option<String> = None;

    // Check 1: a valid `initialize` frame must decode as a request.
    {
        let cx = Cx::for_request();
        match serde_json::to_vec(&build_initialize_request(1)) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                let mut transport = StdioTransport::new(Cursor::new(bytes), Vec::new());
                match transport.recv(&cx) {
                    Ok(JsonRpcMessage::Request(req)) if req.method == "initialize" => {
                        checks.push(CheckResult::pass("decode_round_trip"));
                    }
                    Ok(_) => {
                        protocol_class_failure = true;
                        checks.push(CheckResult::fail(
                            "decode_round_trip",
                            "valid initialize decoded as an unexpected message kind".to_string(),
                        ));
                    }
                    Err(e) => {
                        protocol_class_failure = true;
                        checks.push(CheckResult::fail(
                            "decode_round_trip",
                            format!("valid initialize failed to decode: {e}"),
                        ));
                    }
                }
            }
            Err(e) => {
                protocol_class_failure = true;
                checks.push(CheckResult::fail(
                    "decode_round_trip",
                    format!("could not serialize initialize request: {e}"),
                ));
            }
        }
    }

    // Check 2: the L2 malformed frame MUST be rejected as a protocol error.
    {
        let cx = Cx::for_request();
        let mut transport = StdioTransport::new(Cursor::new(L2_MALFORMED_FIXTURE.to_vec()), Vec::new());
        match transport.recv(&cx) {
            Err(_) => checks.push(CheckResult::pass_with(
                "decode_failure_detected",
                "malformed JSON-RPC correctly surfaced as a protocol decode error".to_string(),
            )),
            Ok(_) => {
                protocol_class_failure = true;
                checks.push(CheckResult::fail(
                    "decode_failure_detected",
                    "malformed JSON-RPC was accepted instead of rejected as a protocol error"
                        .to_string(),
                ));
            }
        }
    }

    // Checks 3-5: full server round-trip (schema negotiation, write, read).
    match run_mcp_session(project_key) {
        Ok(responses) => {
            let by_id = |id: i64| {
                responses
                    .iter()
                    .find(|r| r.get("id").and_then(Value::as_i64) == Some(id))
            };

            if let Some(init) = by_id(1) {
                negotiated_version = init
                    .pointer("/result/protocolVersion")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }

            // Check 3: schema negotiation — tools/list returns a non-empty array.
            match by_id(2)
                .and_then(|r| r.pointer("/result/tools"))
                .and_then(Value::as_array)
            {
                Some(tools) if !tools.is_empty() => checks.push(CheckResult::pass_with(
                    "schema_negotiation",
                    format!("{} tools advertised", tools.len()),
                )),
                _ => checks.push(CheckResult::fail(
                    "schema_negotiation",
                    "tools/list did not return a non-empty tools array".to_string(),
                )),
            }

            // Check 4: write transaction — every write dispatched without error.
            let writes = [
                (3_i64, "ensure_project"),
                (4, "register_agent"),
                (5, "register_agent"),
                (6, "send_message"),
            ];
            let mut write_failure: Option<String> = None;
            for (id, name) in writes {
                match by_id(id) {
                    Some(resp) if !tool_response_is_error(resp) => {}
                    Some(resp) => {
                        write_failure = Some(format!(
                            "{name} (id {id}) returned an error: {}",
                            tool_error_text(resp)
                        ));
                        break;
                    }
                    None => {
                        write_failure = Some(format!("{name} (id {id}) produced no response"));
                        break;
                    }
                }
            }
            match write_failure {
                None => checks.push(CheckResult::pass("write_transaction")),
                Some(detail) => checks.push(CheckResult::fail("write_transaction", detail)),
            }

            // Check 5: read-after-write — list_agents reflects both writes.
            match by_id(7) {
                Some(resp) if !tool_response_is_error(resp) => {
                    if tool_payload_lists_both_agents(resp) {
                        checks.push(CheckResult::pass("read_after_write"));
                    } else {
                        checks.push(CheckResult::fail(
                            "read_after_write",
                            "list_agents did not reflect both registered agents after a write"
                                .to_string(),
                        ));
                    }
                }
                Some(resp) => checks.push(CheckResult::fail(
                    "read_after_write",
                    format!("list_agents returned an error: {}", tool_error_text(resp)),
                )),
                None => checks.push(CheckResult::fail(
                    "read_after_write",
                    "list_agents produced no response".to_string(),
                )),
            }
        }
        Err(e) => {
            checks.push(CheckResult::fail(
                "schema_negotiation",
                format!("mcp session failed: {e}"),
            ));
            checks.push(CheckResult::fail(
                "write_transaction",
                "mcp session did not complete".to_string(),
            ));
            checks.push(CheckResult::fail(
                "read_after_write",
                "mcp session did not complete".to_string(),
            ));
        }
    }

    let ok = checks.iter().all(|c| c.ok);
    McpSelftestReport {
        ok,
        protocol_class_failure,
        negotiated_protocol_version: negotiated_version,
        checks,
    }
}

/// Drive a full MCP lifecycle (initialize → initialized → tools/list →
/// ensure_project → register ×2 → send_message → fetch_inbox) through an
/// in-memory server over the scratch config, returning the JSON-RPC responses.
fn run_mcp_session(project_key: &str) -> Result<Vec<Value>, String> {
    use fastmcp::{Cx, JsonRpcRequest, StdioTransport};
    use std::io::Cursor;

    let requests = vec![
        serde_json::to_vec(&build_initialize_request(1)),
        serde_json::to_vec(&JsonRpcRequest::notification("initialized", None)),
        serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 2_i64)),
        serde_json::to_vec(&tools_call_request(
            "ensure_project",
            json!({ "human_key": project_key }),
            3,
        )),
        serde_json::to_vec(&tools_call_request(
            "register_agent",
            json!({ "project_key": project_key, "program": "claude-code", "model": "selftest", "name": SENDER }),
            4,
        )),
        serde_json::to_vec(&tools_call_request(
            "register_agent",
            json!({ "project_key": project_key, "program": "claude-code", "model": "selftest", "name": RECIPIENT }),
            5,
        )),
        serde_json::to_vec(&tools_call_request(
            "send_message",
            json!({
                "project_key": project_key,
                "sender_name": SENDER,
                "to": [RECIPIENT],
                "subject": "doctor mcp-selftest",
                "body_md": "Synthetic decode probe. Safe to ignore.",
                "ack_required": true,
            }),
            6,
        )),
        // Read-after-write through the MCP transport: list_agents reads the
        // agents table the prior register calls wrote (and which send_message
        // already proved visible by addressing the recipient). This avoids the
        // message-delivery/MVCC visibility nuance of an immediate fetch_inbox.
        serde_json::to_vec(&tools_call_request(
            "list_agents",
            json!({ "project_key": project_key }),
            7,
        )),
    ];

    let mut input = Vec::new();
    for request in requests {
        let mut bytes = request.map_err(|e| format!("serialize mcp request: {e}"))?;
        bytes.push(b'\n');
        input.extend_from_slice(&bytes);
    }

    let server = mcp_agent_mail_server::build_server(&mcp_agent_mail_core::Config::get());
    let writer = SharedBuf::default();
    let output = writer.clone();
    let transport = StdioTransport::new(Cursor::new(input), writer);

    let handle = std::thread::spawn(move || {
        let cx = Cx::for_request();
        server.run_transport_returning_with_cx(&cx, transport);
    });
    handle
        .join()
        .map_err(|_| "mcp session server thread panicked".to_string())?;

    let raw = output.snapshot();
    let text = String::from_utf8(raw).map_err(|e| format!("mcp server output not utf8: {e}"))?;
    let responses = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    Ok(responses)
}

/// True iff a JSON-RPC response carries a transport-level error or a tool
/// result flagged `isError`.
fn tool_response_is_error(resp: &Value) -> bool {
    resp.get("error").is_some()
        || resp.pointer("/result/isError").and_then(Value::as_bool) == Some(true)
}

/// Best-effort extraction of a human-readable error string from a tool/JSON-RPC
/// error response.
fn tool_error_text(resp: &Value) -> String {
    if let Some(text) = resp.pointer("/result/content/0/text").and_then(Value::as_str) {
        return tail(text, 200);
    }
    if let Some(message) = resp.pointer("/error/message").and_then(Value::as_str) {
        return message.to_string();
    }
    "unknown error".to_string()
}

/// True iff a `list_agents` tool response reflects both scratch agents. The
/// check is shape-tolerant: it parses the payload text and confirms both agent
/// names are present (and, when an `agents` array is exposed, that it holds at
/// least two entries).
fn tool_payload_lists_both_agents(resp: &Value) -> bool {
    let Some(text) = resp.pointer("/result/content/0/text").and_then(Value::as_str) else {
        return false;
    };
    if !text.contains(SENDER) || !text.contains(RECIPIENT) {
        return false;
    }
    // If the payload exposes a count/agents array, require at least two.
    if let Ok(payload) = serde_json::from_str::<Value>(text) {
        if payload.get("count").and_then(Value::as_i64).is_some_and(|c| c < 2) {
            return false;
        }
        if payload
            .get("agents")
            .and_then(Value::as_array)
            .is_some_and(|a| a.len() < 2)
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_dimension_maps_taxonomy_to_dimensions() {
        assert_eq!(
            classify_dimension("no such table: messages"),
            Dimension::Schema
        );
        assert_eq!(
            classify_dimension("database is locked"),
            Dimension::Lock
        );
        assert_eq!(
            classify_dimension("database disk image is malformed"),
            Dimension::Corruption
        );
        assert_eq!(
            classify_dimension("Permission denied (os error 13)"),
            Dimension::Permissions
        );
        assert_eq!(
            classify_dimension("attempt to write a readonly database"),
            Dimension::Permissions
        );
    }

    #[test]
    fn dimensions_default_pass_and_first_failure_flips_one() {
        let report = WriteSelftestReport {
            ok: false,
            steps: vec![
                StepResult::passed("ensure_project"),
                StepResult::failed("send_message", "database is locked".to_string()),
            ],
            failing_dimension: Some(Dimension::Lock),
            failed_dimensions: vec![(Dimension::Lock, "database is locked".to_string())],
        };
        let dims = report.dimensions_json();
        assert_eq!(dims["lock"]["status"], "fail");
        assert_eq!(dims["transport"]["status"], "pass");
        assert_eq!(dims["schema"]["status"], "pass");
        assert_eq!(dims["corruption"]["status"], "pass");
        assert_eq!(dims["permissions"]["status"], "pass");
    }

    #[test]
    fn all_pass_report_has_no_failing_dimension() {
        let report = WriteSelftestReport {
            ok: true,
            steps: vec![StepResult::passed("ensure_project")],
            failing_dimension: None,
            failed_dimensions: vec![],
        };
        let value = report.to_json();
        assert_eq!(value["ok"], true);
        assert!(value["failing_dimension"].is_null());
        for dim in Dimension::ALL {
            assert_eq!(value["dimensions"][dim.as_str()]["status"], "pass");
        }
    }

    #[test]
    fn parse_message_id_handles_delivery_shape_and_fallbacks() {
        // Canonical send_message shape.
        assert_eq!(
            parse_message_id(r#"{"deliveries":[{"project":"/p","payload":{"id":99}}],"count":1}"#),
            Some(99)
        );
        // Flat fallbacks.
        assert_eq!(parse_message_id(r#"{"id": 42}"#), Some(42));
        assert_eq!(parse_message_id(r#"{"message_id": 7}"#), Some(7));
        assert_eq!(parse_message_id(r#"{"status":"ok"}"#), None);
        assert_eq!(parse_message_id("not json"), None);
    }

    #[test]
    fn extract_inner_json_tolerates_leading_noise() {
        let out = "warming up\nignored line\n{\"ok\": true, \"steps\": []}\n";
        let value = extract_inner_json(out).expect("should find json");
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn extract_inner_json_returns_none_without_object() {
        assert!(extract_inner_json("no json here\nstill none").is_none());
    }

    // ── C3 — MCP decode self-test ────────────────────────────────────────

    #[test]
    fn mcp_report_protocol_failure_emits_protocol_class_and_guidance() {
        let report = McpSelftestReport {
            ok: false,
            protocol_class_failure: true,
            negotiated_protocol_version: None,
            checks: vec![CheckResult::fail(
                "decode_round_trip",
                "decode failed".to_string(),
            )],
        };
        let value = report.to_json();
        assert_eq!(value["ok"], false);
        assert_eq!(value["failure_class"], "protocol");
        // Guidance must be present, classify the failure as PROTOCOL, and
        // explicitly disclaim a database cause (the ts2 anchor: decode failures
        // were once surfaced as scary DB errors).
        let guidance = value["guidance"].as_str().expect("guidance present");
        assert!(guidance.contains("PROTOCOL"));
        assert!(guidance.to_ascii_lowercase().contains("not a database error"));
    }

    #[test]
    fn mcp_report_tool_dispatch_failure_is_not_classed_protocol() {
        let report = McpSelftestReport {
            ok: false,
            protocol_class_failure: false,
            negotiated_protocol_version: Some("2024-11-05".to_string()),
            checks: vec![CheckResult::fail(
                "write_transaction",
                "send_message error".to_string(),
            )],
        };
        let value = report.to_json();
        assert_eq!(value["failure_class"], "tool_dispatch");
        assert!(value["guidance"].is_null());
    }

    #[test]
    fn mcp_report_all_pass_has_no_failure_class_or_guidance() {
        let report = McpSelftestReport {
            ok: true,
            protocol_class_failure: false,
            negotiated_protocol_version: Some("2024-11-05".to_string()),
            checks: vec![CheckResult::pass("decode_round_trip")],
        };
        let value = report.to_json();
        assert_eq!(value["ok"], true);
        assert!(value["failure_class"].is_null());
        assert!(value["guidance"].is_null());
        assert_eq!(value["expected_protocol_version"], "2024-11-05");
    }

    #[test]
    fn tool_response_is_error_detects_jsonrpc_and_tool_errors() {
        assert!(tool_response_is_error(
            &json!({ "error": { "message": "boom" } })
        ));
        assert!(tool_response_is_error(
            &json!({ "result": { "isError": true, "content": [] } })
        ));
        assert!(!tool_response_is_error(
            &json!({ "result": { "isError": false, "content": [] } })
        ));
        assert!(!tool_response_is_error(&json!({ "result": { "tools": [] } })));
    }

    #[test]
    fn tool_payload_lists_both_agents_requires_both_names() {
        let both = json!({
            "result": { "content": [{ "type": "text", "text":
                "{\"count\": 2, \"agents\": [{\"name\": \"GreenCastle\"}, {\"name\": \"BlueLake\"}]}" }] }
        });
        assert!(tool_payload_lists_both_agents(&both));

        let only_one = json!({
            "result": { "content": [{ "type": "text", "text":
                "{\"count\": 1, \"agents\": [{\"name\": \"GreenCastle\"}]}" }] }
        });
        assert!(!tool_payload_lists_both_agents(&only_one));

        let count_mismatch = json!({
            "result": { "content": [{ "type": "text", "text":
                "{\"count\": 1, \"agents\": [{\"name\": \"GreenCastle\"}, {\"name\": \"BlueLake\"}]}" }] }
        });
        // Names present but count < 2 → still rejected (shape-aware).
        assert!(!tool_payload_lists_both_agents(&count_mismatch));
    }

    #[test]
    fn l2_malformed_fixture_decodes_as_protocol_error() {
        use fastmcp::{StdioTransport, Transport};
        use std::io::Cursor;
        let cx = asupersync::Cx::for_request();
        let mut transport =
            StdioTransport::new(Cursor::new(L2_MALFORMED_FIXTURE.to_vec()), Vec::new());
        // The malformed L2 frame must surface as a decode error, never decode
        // into a usable message — this is the ts2 anchor guard.
        assert!(transport.recv(&cx).is_err());
    }

    #[test]
    fn valid_initialize_round_trips_through_decode() {
        use fastmcp::{JsonRpcMessage, StdioTransport, Transport};
        use std::io::Cursor;
        let cx = asupersync::Cx::for_request();
        let mut bytes = serde_json::to_vec(&build_initialize_request(1)).expect("serialize");
        bytes.push(b'\n');
        let mut transport = StdioTransport::new(Cursor::new(bytes), Vec::new());
        match transport.recv(&cx) {
            Ok(JsonRpcMessage::Request(req)) => assert_eq!(req.method, "initialize"),
            Ok(JsonRpcMessage::Response(_)) => panic!("expected request, decoded a response"),
            Err(_) => panic!("valid initialize must decode without error"),
        }
    }
}
