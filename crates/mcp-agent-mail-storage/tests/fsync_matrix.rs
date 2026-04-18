#![allow(
    clippy::cast_precision_loss,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::too_many_lines
)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use mcp_agent_mail_core::config::Config;
use mcp_agent_mail_storage::{
    MessageBundleBatchEntry, ensure_archive, flush_async_commits, get_recent_commits,
    write_message_batch_bundle, write_message_bundle,
};
use serde::{Deserialize, Serialize};

const SINGLE_RUNS: usize = 9;
const BATCH_RUNS: usize = 9;
const BATCH_SIZE: usize = 100;
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Serialize)]
struct BudgetBand {
    single_p95_ms: u64,
    batch_100_p95_ms: u64,
}

#[derive(Debug, Serialize)]
struct PercentileSummary {
    samples: usize,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}

impl PercentileSummary {
    fn from_micros(mut samples: Vec<u64>) -> Self {
        samples.sort_unstable();
        let p50 = percentile(&samples, 50);
        let p95 = percentile(&samples, 95);
        let p99 = percentile(&samples, 99);
        let max = *samples.last().unwrap_or(&0);
        Self {
            samples: samples.len(),
            p50_ms: micros_to_ms(p50),
            p95_ms: micros_to_ms(p95),
            p99_ms: micros_to_ms(p99),
            max_ms: micros_to_ms(max),
        }
    }
}

#[derive(Debug, Serialize)]
struct CrashProbeSummary {
    canonical_messages: usize,
    git_commits: usize,
    latest_commit_summary: String,
}

#[derive(Debug, Serialize)]
struct ProbeSummary {
    bead_id: &'static str,
    fs_type: String,
    mount_options: String,
    fsync_mode: String,
    storage_root: String,
    budgets: BudgetBand,
    single_message: PercentileSummary,
    batch_100: PercentileSummary,
    crash_probe: CrashProbeSummary,
}

#[derive(Debug, Deserialize, Serialize)]
struct ReadyMarker {
    project_slug: String,
    expected_messages: usize,
}

fn required_env(key: &str) -> String {
    env::var(key).unwrap_or_else(|_| panic!("missing required env var: {key}"))
}

fn required_path(key: &str) -> PathBuf {
    PathBuf::from(required_env(key))
}

fn parse_budget(key: &str) -> u64 {
    required_env(key)
        .parse::<u64>()
        .unwrap_or_else(|_| panic!("invalid integer budget in {key}"))
}

fn test_config(root: &Path) -> Config {
    Config {
        storage_root: root.to_path_buf(),
        ..Config::default()
    }
}

fn micros_to_ms(value: u64) -> f64 {
    value as f64 / 1000.0
}

fn duration_to_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn percentile(sorted_samples: &[u64], pct: usize) -> u64 {
    if sorted_samples.is_empty() {
        return 0;
    }
    let rank = (sorted_samples.len() * pct).div_ceil(100).saturating_sub(1);
    sorted_samples[rank.min(sorted_samples.len() - 1)]
}

fn make_message(
    message_id: usize,
    project: &str,
    thread_id: &str,
    subject: &str,
) -> serde_json::Value {
    let seconds = message_id % 60;
    let minutes = (message_id / 60) % 60;
    serde_json::json!({
        "id": i64::try_from(message_id).unwrap_or(i64::MAX),
        "subject": format!("{subject} #{message_id}"),
        "created_ts": format!("2026-04-18T12:{minutes:02}:{seconds:02}Z"),
        "thread_id": thread_id,
        "project": project,
        "to": ["RecipientAgent"],
    })
}

fn measure_single_message(storage_root: &Path) -> PercentileSummary {
    let root = storage_root.join("single_message");
    fs::create_dir_all(&root).expect("create single_message root");
    let config = test_config(&root);
    let archive = ensure_archive(&config, "fsync-single").expect("ensure archive");
    let recipients = vec!["RecipientAgent".to_string()];
    let extra_paths: Vec<String> = Vec::new();
    let mut samples = Vec::with_capacity(SINGLE_RUNS);

    eprintln!("perf.filesystem.bench_start {{ operation: \"single_message\" }}");

    for idx in 0..=SINGLE_RUNS {
        let message = make_message(
            idx + 1,
            "fsync-single",
            &format!("FSYNC-SINGLE-{idx}"),
            "FSync single message probe",
        );
        let started = Instant::now();
        write_message_bundle(
            &archive,
            &config,
            &message,
            "Single-message fsync probe payload.",
            "SenderAgent",
            &recipients,
            &extra_paths,
            None,
        )
        .expect("write single message bundle");
        flush_async_commits();
        if idx > 0 {
            samples.push(duration_to_micros(started.elapsed()));
        }
    }

    PercentileSummary::from_micros(samples)
}

fn measure_batch_message(storage_root: &Path) -> PercentileSummary {
    let root = storage_root.join("batch_message");
    fs::create_dir_all(&root).expect("create batch_message root");
    let config = test_config(&root);
    let archive = ensure_archive(&config, "fsync-batch").expect("ensure archive");
    let recipients = vec!["RecipientAgent".to_string()];
    let extra_paths: Vec<String> = Vec::new();
    let mut samples = Vec::with_capacity(BATCH_RUNS);
    let mut next_message_id = 10_000usize;

    eprintln!("perf.filesystem.bench_start {{ operation: \"batch_100\" }}");

    for run_idx in 0..=BATCH_RUNS {
        let thread_id = format!("FSYNC-BATCH-{run_idx}");
        let messages = (0..BATCH_SIZE)
            .map(|offset| {
                let message_id = next_message_id + offset;
                make_message(
                    message_id,
                    "fsync-batch",
                    &thread_id,
                    "FSync batch message probe",
                )
            })
            .collect::<Vec<_>>();
        next_message_id += BATCH_SIZE;
        let entries = messages
            .iter()
            .map(|message| MessageBundleBatchEntry {
                message,
                body_md: "Batch fsync probe payload.",
                sender: "SenderAgent",
                recipients: &recipients,
                extra_paths: &extra_paths,
            })
            .collect::<Vec<_>>();

        let started = Instant::now();
        write_message_batch_bundle(&archive, &config, &entries, None)
            .expect("write batch message bundle");
        flush_async_commits();
        if run_idx > 0 {
            samples.push(duration_to_micros(started.elapsed()));
        }
    }

    PercentileSummary::from_micros(samples)
}

fn current_exe() -> PathBuf {
    env::current_exe().expect("resolve current test binary")
}

fn wait_for_ready_file(child: &mut Child, path: &Path) {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() <= deadline {
        if path.is_file() {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll child process") {
            panic!(
                "crash probe child exited before ready marker (status: {status}) at {}",
                path.display()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
    panic!("timed out waiting for ready marker at {}", path.display());
}

fn count_canonical_messages(root: &Path) -> usize {
    if !root.is_dir() {
        return 0;
    }
    let mut total = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("read_dir failed for {}: {err}", dir.display()));
        for entry in entries {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("entry file_type");
            if file_type.is_dir() {
                if path.file_name().and_then(|name| name.to_str()) == Some("threads") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                total += 1;
            }
        }
    }
    total
}

fn run_crash_probe(storage_root: &Path) -> CrashProbeSummary {
    const PROJECT_SLUG: &str = "fsync-crash";
    let root = storage_root.join("crash_probe");
    fs::create_dir_all(&root).expect("create crash_probe root");
    let ready_file = root.join("ready.json");
    let mut child = Command::new(current_exe())
        .arg("--ignored")
        .arg("--exact")
        .arg("archive_fsync_matrix_child_writer")
        .arg("--nocapture")
        .env("AM_FSYNC_MATRIX_CHILD", "1")
        .env("AM_FSYNC_MATRIX_CHILD_ROOT", &root)
        .env("AM_FSYNC_MATRIX_CHILD_READY_FILE", &ready_file)
        .spawn()
        .expect("spawn crash probe child");

    wait_for_ready_file(&mut child, &ready_file);
    child.kill().expect("kill crash probe child");
    let _status = child.wait().expect("wait crash probe child");

    let ready: ReadyMarker = serde_json::from_str(
        &fs::read_to_string(&ready_file).expect("read crash probe ready file"),
    )
    .expect("parse crash probe ready file");
    assert_eq!(ready.project_slug, PROJECT_SLUG);

    let config = test_config(&root);
    let archive = ensure_archive(&config, PROJECT_SLUG).expect("reopen crash probe archive");
    let canonical_messages = count_canonical_messages(&archive.root.join("messages"));
    let commits = get_recent_commits(&archive, 5, None).expect("read recent commits");

    assert_eq!(
        canonical_messages, ready.expected_messages,
        "post-kill canonical message count mismatch after flush_async_commits"
    );
    assert!(
        !commits.is_empty(),
        "expected at least one persisted commit after crash probe"
    );

    CrashProbeSummary {
        canonical_messages,
        git_commits: commits.len(),
        latest_commit_summary: commits[0].summary.clone(),
    }
}

fn write_summary(path: &Path, summary: &ProbeSummary) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create artifact parent");
    }
    let json = serde_json::to_string_pretty(summary).expect("serialize summary");
    fs::write(path, format!("{json}\n")).expect("write summary");
}

#[test]
#[ignore = "run via scripts/bench_archive_fsync_matrix.sh"]
fn archive_fsync_matrix_probe() {
    let fs_type = required_env("AM_FSYNC_MATRIX_FS_LABEL");
    let mount_options = required_env("AM_FSYNC_MATRIX_MOUNT_OPTIONS");
    let fsync_mode = required_env("AM_FSYNC_MATRIX_FSYNC_MODE");
    let storage_root = required_path("AM_FSYNC_MATRIX_STORAGE_ROOT");
    let artifact_dir = required_path("AM_FSYNC_MATRIX_ARTIFACT_DIR");
    let single_budget_ms = parse_budget("AM_FSYNC_MATRIX_SINGLE_P95_BUDGET_MS");
    let batch_budget_ms = parse_budget("AM_FSYNC_MATRIX_BATCH_100_P95_BUDGET_MS");

    fs::create_dir_all(&storage_root).expect("create storage root");
    fs::create_dir_all(&artifact_dir).expect("create artifact dir");

    eprintln!(
        "perf.filesystem.detected {{ fs_type: \"{fs_type}\", mount_options: \"{mount_options}\" }}"
    );
    eprintln!("perf.filesystem.fsync_mode {{ mode: \"{fsync_mode}\" }}");

    let single_message = measure_single_message(&storage_root);
    eprintln!(
        "perf.filesystem.bench_complete {{ operation: \"single_message\", fs_type: \"{fs_type}\", p50_ms: {:.3}, p95_ms: {:.3}, p99_ms: {:.3}, vs_budget: \"{}\" }}",
        single_message.p50_ms,
        single_message.p95_ms,
        single_message.p99_ms,
        if single_message.p95_ms <= single_budget_ms as f64 {
            "within_budget"
        } else {
            "over_budget"
        }
    );

    let batch_100 = measure_batch_message(&storage_root);
    eprintln!(
        "perf.filesystem.bench_complete {{ operation: \"batch_100\", fs_type: \"{fs_type}\", p50_ms: {:.3}, p95_ms: {:.3}, p99_ms: {:.3}, vs_budget: \"{}\" }}",
        batch_100.p50_ms,
        batch_100.p95_ms,
        batch_100.p99_ms,
        if batch_100.p95_ms <= batch_budget_ms as f64 {
            "within_budget"
        } else {
            "over_budget"
        }
    );

    let crash_probe = run_crash_probe(&storage_root);
    let summary = ProbeSummary {
        bead_id: "br-8qdh0.11",
        fs_type,
        mount_options,
        fsync_mode,
        storage_root: storage_root.display().to_string(),
        budgets: BudgetBand {
            single_p95_ms: single_budget_ms,
            batch_100_p95_ms: batch_budget_ms,
        },
        single_message,
        batch_100,
        crash_probe,
    };
    write_summary(&artifact_dir.join("summary.json"), &summary);

    assert!(
        summary.single_message.p95_ms <= single_budget_ms as f64,
        "single-message p95 {:.3}ms exceeded budget {}ms on {}",
        summary.single_message.p95_ms,
        single_budget_ms,
        summary.fs_type
    );
    assert!(
        summary.batch_100.p95_ms <= batch_budget_ms as f64,
        "batch-100 p95 {:.3}ms exceeded budget {}ms on {}",
        summary.batch_100.p95_ms,
        batch_budget_ms,
        summary.fs_type
    );
}

#[test]
#[ignore = "spawned by archive_fsync_matrix_probe"]
fn archive_fsync_matrix_child_writer() {
    if env::var_os("AM_FSYNC_MATRIX_CHILD").is_none() {
        return;
    }

    const PROJECT_SLUG: &str = "fsync-crash";
    let root = required_path("AM_FSYNC_MATRIX_CHILD_ROOT");
    let ready_file = required_path("AM_FSYNC_MATRIX_CHILD_READY_FILE");
    fs::create_dir_all(&root).expect("create child root");

    let config = test_config(&root);
    let archive = ensure_archive(&config, PROJECT_SLUG).expect("ensure crash probe archive");
    let recipients = vec!["RecipientAgent".to_string()];
    let extra_paths: Vec<String> = Vec::new();
    let messages = (0..BATCH_SIZE)
        .map(|offset| {
            make_message(
                90_000 + offset,
                PROJECT_SLUG,
                "FSYNC-CRASH-BATCH",
                "Crash probe message",
            )
        })
        .collect::<Vec<_>>();
    let entries = messages
        .iter()
        .map(|message| MessageBundleBatchEntry {
            message,
            body_md: "Crash-probe payload.",
            sender: "SenderAgent",
            recipients: &recipients,
            extra_paths: &extra_paths,
        })
        .collect::<Vec<_>>();

    write_message_batch_bundle(&archive, &config, &entries, None)
        .expect("write crash probe batch bundle");
    flush_async_commits();

    let ready = ReadyMarker {
        project_slug: PROJECT_SLUG.to_string(),
        expected_messages: BATCH_SIZE,
    };
    fs::write(
        &ready_file,
        format!(
            "{}\n",
            serde_json::to_string(&ready).expect("serialize crash probe ready marker")
        ),
    )
    .expect("write crash probe ready marker");

    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
