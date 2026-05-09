//! Known-bad git stress harness for the git 2.51.0 index-race mitigation.
//!
//! The four exported tests are intentionally the only `#[test]` functions in
//! this file. Infrastructure checks for the shim, threshold TOML, and artifact
//! writer are invoked from `scenario_a_clean_baseline` to preserve the bead's
//! exact four-scenario contract.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::manual_let_else,
    clippy::missing_const_for_fn,
    clippy::needless_collect,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use chrono::{SecondsFormat, Utc};
use mcp_agent_mail_core::config::Config;
use mcp_agent_mail_core::models::{VALID_ADJECTIVES, VALID_NOUNS};
use mcp_agent_mail_core::{GitCmd, GitRunOutcome};
use mcp_agent_mail_db::{DbPool, DbPoolConfig, micros_to_iso, queries};
use mcp_agent_mail_storage::{
    ProjectArchive, ensure_archive, flush_async_commits, write_message_bundle,
};
use serde_json::{Value, json};
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scenario {
    A,
    B,
    C,
    D,
}

impl Scenario {
    const fn key(self) -> &'static str {
        match self {
            Self::A => "scenario_a",
            Self::B => "scenario_b",
            Self::C => "scenario_c",
            Self::D => "scenario_d",
        }
    }

    const fn test_name(self) -> &'static str {
        match self {
            Self::A => "scenario_a_clean_baseline",
            Self::B => "scenario_b_synthetic_racer",
            Self::C => "scenario_c_real_2510_gated",
            Self::D => "scenario_d_real_2510_no_flock_gated",
        }
    }

    const fn display(self) -> &'static str {
        match self {
            Self::A => "clean baseline",
            Self::B => "synthetic racer",
            Self::C => "real git 2.51.0",
            Self::D => "real git 2.51.0 without flock",
        }
    }
}

#[derive(Clone, Debug)]
struct ScenarioSettings {
    scenario: Scenario,
    skip_flock: bool,
    skip_mutex: bool,
}

#[derive(Clone, Debug, Default)]
struct WorkloadStats {
    total_messages: u64,
    archive_errors: u64,
    logical_git_calls: u64,
    segfault_retries: u64,
    git_errors: u64,
    archive_corruption_count: u64,
    libgit2_retries: u64,
    duration_ms: u64,
}

impl WorkloadStats {
    fn merge(&mut self, other: Self) {
        self.total_messages += other.total_messages;
        self.archive_errors += other.archive_errors;
        self.logical_git_calls += other.logical_git_calls;
        self.segfault_retries += other.segfault_retries;
        self.git_errors += other.git_errors;
        self.archive_corruption_count += other.archive_corruption_count;
        self.libgit2_retries += other.libgit2_retries;
        self.duration_ms += other.duration_ms;
    }

    fn retry_rate(&self) -> f64 {
        if self.logical_git_calls == 0 {
            0.0
        } else {
            self.segfault_retries as f64 / self.logical_git_calls as f64
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Threshold {
    retry_rate_min: Option<f64>,
    retry_rate_max: Option<f64>,
    archive_corruption_min: Option<u64>,
    archive_corruption_max: Option<u64>,
    libgit2_retries_max: Option<u64>,
}

type EventSink = Arc<Mutex<Vec<Value>>>;

#[test]
fn scenario_a_clean_baseline() {
    if helper_requested_for(Scenario::A) {
        assert_test_infrastructure();
        run_scenario_direct(ScenarioSettings {
            scenario: Scenario::A,
            skip_flock: false,
            skip_mutex: false,
        });
        return;
    }

    run_scenario_in_child(Scenario::A, |cmd| {
        cmd.env_remove("AM_GIT_BINARY");
        cmd.env_remove("AM_TEST_RACER_PROB");
        cmd.env_remove("AM_TEST_RACER_STATE");
        cmd.env_remove("AM_TEST_RACER_EVENT_LOG");
    });
}

#[test]
fn scenario_b_synthetic_racer() {
    if helper_requested_for(Scenario::B) {
        run_scenario_direct(ScenarioSettings {
            scenario: Scenario::B,
            skip_flock: false,
            skip_mutex: false,
        });
        return;
    }

    if !extended_gate_enabled() {
        eprintln!("scenario_b_synthetic_racer skipped: set AM_TEST_GIT_251=1");
        return;
    }

    let shim = shim_path();
    assert!(shim.exists(), "missing shim at {}", shim.display());
    run_scenario_in_child(Scenario::B, |cmd| {
        let state = std::env::temp_dir().join(format!("am-git-racer-{}.state", unique_suffix()));
        let event_log =
            std::env::temp_dir().join(format!("am-git-racer-{}.jsonl", unique_suffix()));
        cmd.env("AM_GIT_BINARY", &shim);
        cmd.env("AM_TEST_RACER_PROB", "0.05");
        cmd.env("AM_TEST_RACER_TRIGGERS", "update-ref,commit");
        cmd.env("AM_TEST_RACER_STATE", state);
        cmd.env("AM_TEST_RACER_EVENT_LOG", event_log);
        cmd.env("AM_TEST_RACER_SCENARIO", Scenario::B.key());
    });
}

#[test]
fn scenario_c_real_2510_gated() {
    if helper_requested_for(Scenario::C) {
        run_scenario_direct(ScenarioSettings {
            scenario: Scenario::C,
            skip_flock: false,
            skip_mutex: false,
        });
        return;
    }

    let Some(git_binary) = real_git_2510_gate(Scenario::C) else {
        return;
    };
    run_scenario_in_child(Scenario::C, |cmd| {
        cmd.env("AM_GIT_BINARY", git_binary);
    });
}

#[test]
fn scenario_d_real_2510_no_flock_gated() {
    if helper_requested_for(Scenario::D) {
        run_scenario_direct(ScenarioSettings {
            scenario: Scenario::D,
            skip_flock: true,
            skip_mutex: true,
        });
        return;
    }

    let Some(git_binary) = real_git_2510_gate(Scenario::D) else {
        return;
    };
    run_scenario_in_child(Scenario::D, |cmd| {
        cmd.env("AM_GIT_BINARY", git_binary);
        cmd.env("AM_GIT_FLOCK_DISABLED", "1");
    });
}

fn run_scenario_direct(settings: ScenarioSettings) {
    let started = Instant::now();
    let events = EventSink::default();
    emit_event(
        &events,
        settings.scenario,
        "info",
        "scenario_started",
        json!({ "display": settings.scenario.display() }),
        None,
    );

    let mut stats = run_combined_workload(&settings, &events);
    stats.duration_ms = duration_ms_u64(started.elapsed());
    assert_thresholds(settings.scenario, &stats);

    emit_event(
        &events,
        settings.scenario,
        "pass",
        "scenario_passed",
        summary_json(settings.scenario, &stats),
        Some(stats.duration_ms),
    );
    write_artifacts(settings.scenario, &stats, &events).expect("write artifacts");
}

fn run_combined_workload(settings: &ScenarioSettings, events: &EventSink) -> WorkloadStats {
    let tmp = TempDir::new().expect("tempdir");
    let config = test_config(tmp.path());
    let pool = make_pool(&tmp);

    let mut stats = WorkloadStats::default();
    let mut archives = Vec::new();
    stats.merge(run_message_pipeline(
        settings,
        events,
        &config,
        &pool,
        &mut archives,
    ));
    stats.merge(run_multi_project_pipeline(
        settings,
        events,
        &config,
        &pool,
        &mut archives,
    ));
    flush_async_commits();
    stats.archive_corruption_count += count_archive_corruption(&archives);
    stats
}

fn run_message_pipeline(
    settings: &ScenarioSettings,
    events: &EventSink,
    config: &Config,
    pool: &DbPool,
    archives: &mut Vec<PathBuf>,
) -> WorkloadStats {
    let n_agents = 30;
    let msgs_per_agent = 5;
    let human_key = unique_human_key("known-bad-git-30");
    let pool_setup = pool.clone();
    let (project_id, project_slug, agent_ids) = block_on(|cx| async move {
        let project = match queries::ensure_project(&cx, &pool_setup, &human_key).await {
            Outcome::Ok(row) => row,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let project_id = project.id.expect("project id");
        let mut ids = Vec::new();
        for i in 0..n_agents {
            let name = agent_name(i);
            let agent = match queries::register_agent(
                &cx,
                &pool_setup,
                project_id,
                &name,
                "known-bad-git-stress",
                "test-model",
                Some("known-bad git stress agent"),
                None,
                None,
            )
            .await
            {
                Outcome::Ok(row) => row,
                other => panic!("register agent {name} failed: {other:?}"),
            };
            ids.push((agent.id.expect("agent id"), name));
        }
        (project_id, project.slug, ids)
    });

    let archive = ensure_archive(config, &project_slug).expect("ensure archive");
    archives.push(archive.repo_root.clone());

    let barrier = Arc::new(Barrier::new(n_agents));
    let successes = Arc::new(AtomicU64::new(0));
    let archive_errors = Arc::new(AtomicU64::new(0));
    let git_calls = Arc::new(AtomicU64::new(0));
    let git_retries = Arc::new(AtomicU64::new(0));
    let git_errors = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..n_agents)
        .map(|i| {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agent_ids = agent_ids.clone();
            let barrier = Arc::clone(&barrier);
            let successes = Arc::clone(&successes);
            let archive_errors = Arc::clone(&archive_errors);
            let git_calls = Arc::clone(&git_calls);
            let git_retries = Arc::clone(&git_retries);
            let git_errors = Arc::clone(&git_errors);
            let events = Arc::clone(events);
            let settings = settings.clone();

            std::thread::Builder::new()
                .name(format!("kbg-agent-{i}"))
                .spawn(move || {
                    barrier.wait();
                    for msg_idx in 0..msgs_per_agent {
                        let (sender_id, sender_name) = &agent_ids[i];
                        let recipient_idx = (i + msg_idx + 1) % n_agents;
                        let (recipient_id, recipient_name) = &agent_ids[recipient_idx];
                        let thread_id = format!("kbg-t{i}-m{msg_idx}");
                        let body = format!(
                            "Known-bad git stress body {msg_idx} from {i} to {recipient_idx}"
                        );

                        let message = block_on_with_retry(5, |cx| {
                            let pool = pool.clone();
                            let body = body.clone();
                            let thread_id = thread_id.clone();
                            async move {
                                queries::create_message_with_recipients(
                                    &cx,
                                    &pool,
                                    project_id,
                                    *sender_id,
                                    &format!("Known-bad git message {msg_idx}"),
                                    &body,
                                    Some(&thread_id),
                                    "normal",
                                    false,
                                    "[]",
                                    &[(*recipient_id, "to")],
                                )
                                .await
                            }
                        });

                        let msg_json = serde_json::json!({
                            "id": message.id.expect("message id"),
                            "subject": format!("Known-bad git message {msg_idx}"),
                            "thread_id": thread_id,
                            "created_ts": micros_to_iso(message.created_ts),
                        });

                        if write_message_bundle(
                            &archive,
                            &config,
                            &msg_json,
                            &body,
                            sender_name,
                            std::slice::from_ref(recipient_name),
                            &[],
                            None,
                        )
                        .is_ok()
                        {
                            successes.fetch_add(1, Ordering::Relaxed);
                        } else {
                            archive_errors.fetch_add(1, Ordering::Relaxed);
                        }

                        let ref_name = format!("refs/heads/am-kbg/pipeline/{i}/{msg_idx}");
                        let probe = run_git_probe(&archive, &settings, &events, &ref_name);
                        git_calls.fetch_add(1, Ordering::Relaxed);
                        git_retries.fetch_add(probe.segfault_retries, Ordering::Relaxed);
                        if !probe.success {
                            git_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
                .expect("spawn pipeline worker")
        })
        .collect();

    for handle in handles {
        handle.join().expect("pipeline worker panicked");
    }

    WorkloadStats {
        total_messages: successes.load(Ordering::Relaxed),
        archive_errors: archive_errors.load(Ordering::Relaxed),
        logical_git_calls: git_calls.load(Ordering::Relaxed),
        segfault_retries: git_retries.load(Ordering::Relaxed),
        git_errors: git_errors.load(Ordering::Relaxed),
        ..WorkloadStats::default()
    }
}

fn run_multi_project_pipeline(
    settings: &ScenarioSettings,
    events: &EventSink,
    config: &Config,
    pool: &DbPool,
    archives: &mut Vec<PathBuf>,
) -> WorkloadStats {
    let n_projects = 10;
    let agents_per_project = 5;
    let msgs_per_agent = 3;
    let pool_setup = pool.clone();
    let mut project_data = Vec::new();

    for p in 0..n_projects {
        let human_key = unique_human_key(&format!("known-bad-git-multi-{p}"));
        let pool_clone = pool_setup.clone();
        let (project_id, slug, agents) = block_on(|cx| async move {
            let project = match queries::ensure_project(&cx, &pool_clone, &human_key).await {
                Outcome::Ok(row) => row,
                other => panic!("ensure_project p{p} failed: {other:?}"),
            };
            let pid = project.id.expect("project id");
            let mut agents = Vec::new();
            for a in 0..agents_per_project {
                let name = agent_name(p * agents_per_project + a + 100);
                let agent = match queries::register_agent(
                    &cx,
                    &pool_clone,
                    pid,
                    &name,
                    "known-bad-git-stress",
                    "test-model",
                    Some("known-bad multi-project stress"),
                    None,
                    None,
                )
                .await
                {
                    Outcome::Ok(row) => row,
                    other => panic!("register agent {name} p{p} failed: {other:?}"),
                };
                agents.push((agent.id.expect("agent id"), name));
            }
            (pid, project.slug, agents)
        });
        let archive = ensure_archive(config, &slug).expect("ensure archive");
        archives.push(archive.repo_root.clone());
        project_data.push((project_id, slug, agents, archive));
    }

    let workers = n_projects * agents_per_project;
    let barrier = Arc::new(Barrier::new(workers));
    let successes = Arc::new(AtomicU64::new(0));
    let archive_errors = Arc::new(AtomicU64::new(0));
    let git_calls = Arc::new(AtomicU64::new(0));
    let git_retries = Arc::new(AtomicU64::new(0));
    let git_errors = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    for (p_idx, (project_id, _slug, agents, archive)) in project_data.iter().enumerate() {
        for (a_idx, (sender_id, sender_name)) in agents.iter().enumerate() {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agents = agents.clone();
            let barrier = Arc::clone(&barrier);
            let successes = Arc::clone(&successes);
            let archive_errors = Arc::clone(&archive_errors);
            let git_calls = Arc::clone(&git_calls);
            let git_retries = Arc::clone(&git_retries);
            let git_errors = Arc::clone(&git_errors);
            let events = Arc::clone(events);
            let settings = settings.clone();
            let project_id = *project_id;
            let sender_id = *sender_id;
            let sender_name = sender_name.clone();

            handles.push(
                std::thread::Builder::new()
                    .name(format!("kbg-p{p_idx}-a{a_idx}"))
                    .spawn(move || {
                        barrier.wait();
                        for m in 0..msgs_per_agent {
                            let recipient_idx = (a_idx + m + 1) % agents.len();
                            let (recipient_id, recipient_name) = &agents[recipient_idx];
                            let thread_id = format!("kbg-mp-p{p_idx}-a{a_idx}-m{m}");
                            let body = format!("Known-bad multi body p{p_idx} a{a_idx} m{m}");

                            let msg = block_on_with_retry(5, |cx| {
                                let pool = pool.clone();
                                let body = body.clone();
                                let thread_id = thread_id.clone();
                                async move {
                                    queries::create_message_with_recipients(
                                        &cx,
                                        &pool,
                                        project_id,
                                        sender_id,
                                        &format!("Known-bad multi msg {m}"),
                                        &body,
                                        Some(&thread_id),
                                        "normal",
                                        false,
                                        "[]",
                                        &[(*recipient_id, "to")],
                                    )
                                    .await
                                }
                            });

                            let msg_json = serde_json::json!({
                                "id": msg.id.expect("msg id"),
                                "subject": format!("Known-bad multi msg {m}"),
                                "thread_id": thread_id,
                                "created_ts": micros_to_iso(msg.created_ts),
                            });

                            if write_message_bundle(
                                &archive,
                                &config,
                                &msg_json,
                                &body,
                                &sender_name,
                                std::slice::from_ref(recipient_name),
                                &[],
                                None,
                            )
                            .is_ok()
                            {
                                successes.fetch_add(1, Ordering::Relaxed);
                            } else {
                                archive_errors.fetch_add(1, Ordering::Relaxed);
                            }

                            let ref_name = format!("refs/heads/am-kbg/multi/{p_idx}/{a_idx}/{m}");
                            let probe = run_git_probe(&archive, &settings, &events, &ref_name);
                            git_calls.fetch_add(1, Ordering::Relaxed);
                            git_retries.fetch_add(probe.segfault_retries, Ordering::Relaxed);
                            if !probe.success {
                                git_errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    })
                    .expect("spawn multi-project worker"),
            );
        }
    }

    for handle in handles {
        handle.join().expect("multi-project worker panicked");
    }

    WorkloadStats {
        total_messages: successes.load(Ordering::Relaxed),
        archive_errors: archive_errors.load(Ordering::Relaxed),
        logical_git_calls: git_calls.load(Ordering::Relaxed),
        segfault_retries: git_retries.load(Ordering::Relaxed),
        git_errors: git_errors.load(Ordering::Relaxed),
        ..WorkloadStats::default()
    }
}

#[derive(Clone, Copy, Debug)]
struct GitProbeOutcome {
    success: bool,
    segfault_retries: u64,
}

fn run_git_probe(
    archive: &ProjectArchive,
    settings: &ScenarioSettings,
    events: &EventSink,
    ref_name: &str,
) -> GitProbeOutcome {
    const MAX_RETRIES: u32 = 3;
    let args_hash = args_hash(&["update-ref", "-d", ref_name]);
    let repo_slug = archive.slug.clone();
    let start = Instant::now();
    let mut segfault_retries = 0;

    for attempt in 0..=MAX_RETRIES {
        let mut cmd = GitCmd::new(&archive.repo_root)
            .args(["update-ref", "-d", ref_name])
            .timeout(Duration::from_secs(15));
        if settings.skip_flock {
            cmd = cmd.skip_flock();
        }
        if settings.skip_mutex {
            cmd = cmd.skip_mutex();
        }

        match cmd.run_once() {
            GitRunOutcome::Finished(output) if output.status.success() => {
                if segfault_retries > 0 {
                    emit_event(
                        events,
                        settings.scenario,
                        "info",
                        "git_segfault_retry_succeeded",
                        json!({
                            "caller": "stress_pipeline_known_bad_git",
                            "attempt_n": attempt,
                            "exit_code": 0,
                            "repo_slug": repo_slug,
                            "args_hash": args_hash,
                        }),
                        Some(duration_ms_u64(start.elapsed())),
                    );
                }
                return GitProbeOutcome {
                    success: true,
                    segfault_retries,
                };
            }
            GitRunOutcome::Finished(output) => {
                emit_event(
                    events,
                    settings.scenario,
                    "error",
                    "git_probe_failed",
                    json!({
                        "caller": "stress_pipeline_known_bad_git",
                        "attempt_n": attempt,
                        "exit_code": output.status.code().unwrap_or(-1),
                        "repo_slug": repo_slug,
                        "args_hash": args_hash,
                        "stderr": String::from_utf8_lossy(&output.stderr),
                    }),
                    Some(duration_ms_u64(start.elapsed())),
                );
                return GitProbeOutcome {
                    success: false,
                    segfault_retries,
                };
            }
            GitRunOutcome::SegfaultLike { signal } => {
                segfault_retries += 1;
                emit_event(
                    events,
                    settings.scenario,
                    "warn",
                    "git_segfault_retry",
                    json!({
                        "caller": "stress_pipeline_known_bad_git",
                        "attempt_n": attempt + 1,
                        "exit_code": 128 + signal,
                        "repo_slug": repo_slug,
                        "args_hash": args_hash,
                    }),
                    Some(duration_ms_u64(start.elapsed())),
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            GitRunOutcome::OtherSignal { signal } => {
                emit_event(
                    events,
                    settings.scenario,
                    "error",
                    "git_probe_other_signal",
                    json!({
                        "caller": "stress_pipeline_known_bad_git",
                        "attempt_n": attempt,
                        "exit_code": 128 + signal,
                        "repo_slug": repo_slug,
                        "args_hash": args_hash,
                    }),
                    Some(duration_ms_u64(start.elapsed())),
                );
                return GitProbeOutcome {
                    success: false,
                    segfault_retries,
                };
            }
            GitRunOutcome::Timeout { after } => {
                emit_event(
                    events,
                    settings.scenario,
                    "error",
                    "git_probe_timeout",
                    json!({
                        "caller": "stress_pipeline_known_bad_git",
                        "attempt_n": attempt,
                        "exit_code": -1,
                        "repo_slug": repo_slug,
                        "args_hash": args_hash,
                        "timeout_ms": duration_ms_u64(after),
                    }),
                    Some(duration_ms_u64(start.elapsed())),
                );
                return GitProbeOutcome {
                    success: false,
                    segfault_retries,
                };
            }
            GitRunOutcome::Error(error) => {
                emit_event(
                    events,
                    settings.scenario,
                    "error",
                    "git_probe_io_error",
                    json!({
                        "caller": "stress_pipeline_known_bad_git",
                        "attempt_n": attempt,
                        "exit_code": -1,
                        "repo_slug": repo_slug,
                        "args_hash": args_hash,
                        "error": error.to_string(),
                    }),
                    Some(duration_ms_u64(start.elapsed())),
                );
                return GitProbeOutcome {
                    success: false,
                    segfault_retries,
                };
            }
        }
    }

    emit_event(
        events,
        settings.scenario,
        "error",
        "git_segfault_retry_exhausted",
        json!({
            "caller": "stress_pipeline_known_bad_git",
            "attempt_n": MAX_RETRIES + 1,
            "exit_code": 139,
            "repo_slug": repo_slug,
            "args_hash": args_hash,
        }),
        Some(duration_ms_u64(start.elapsed())),
    );
    GitProbeOutcome {
        success: false,
        segfault_retries,
    }
}

fn count_archive_corruption(repos: &[PathBuf]) -> u64 {
    repos
        .iter()
        .filter(|repo_root| {
            let Ok(repo) = git2::Repository::open(repo_root) else {
                return true;
            };
            repo.references().is_err()
        })
        .count() as u64
}

fn assert_thresholds(scenario: Scenario, stats: &WorkloadStats) {
    let thresholds = load_thresholds().expect("load stress thresholds");
    let threshold = thresholds
        .get(scenario.key())
        .unwrap_or_else(|| panic!("missing threshold for {}", scenario.key()));

    if let Some(min) = threshold.retry_rate_min {
        assert!(
            stats.retry_rate() >= min,
            "{} retry_rate {:.4} below min {min:.4}",
            scenario.key(),
            stats.retry_rate()
        );
    }
    if let Some(max) = threshold.retry_rate_max {
        assert!(
            stats.retry_rate() <= max,
            "{} retry_rate {:.4} above max {max:.4}",
            scenario.key(),
            stats.retry_rate()
        );
    }
    if let Some(min) = threshold.archive_corruption_min {
        assert!(
            stats.archive_corruption_count >= min,
            "{} archive_corruption_count {} below min {min}",
            scenario.key(),
            stats.archive_corruption_count
        );
    }
    if let Some(max) = threshold.archive_corruption_max {
        assert!(
            stats.archive_corruption_count <= max,
            "{} archive_corruption_count {} above max {max}",
            scenario.key(),
            stats.archive_corruption_count
        );
    }
    if let Some(max) = threshold.libgit2_retries_max {
        assert!(
            stats.libgit2_retries <= max,
            "{} libgit2_retries {} above max {max}",
            scenario.key(),
            stats.libgit2_retries
        );
    }
    assert_eq!(stats.archive_errors, 0, "archive writes must not fail");
    assert_eq!(stats.git_errors, 0, "git probes must not fail");
}

fn load_thresholds() -> Result<BTreeMap<String, Threshold>, String> {
    parse_thresholds(&fs::read_to_string(threshold_path()).map_err(|e| e.to_string())?)
}

fn parse_thresholds(raw: &str) -> Result<BTreeMap<String, Threshold>, String> {
    let value: toml::Value =
        toml::from_str(raw).map_err(|e| format!("invalid thresholds TOML: {e}"))?;
    let table = value
        .as_table()
        .ok_or_else(|| "threshold root must be a table".to_string())?;
    let mut out = BTreeMap::new();

    for key in ["scenario_a", "scenario_b", "scenario_c", "scenario_d"] {
        let entry = table
            .get(key)
            .and_then(toml::Value::as_table)
            .ok_or_else(|| format!("missing threshold table [{key}]"))?;
        let threshold = Threshold {
            retry_rate_min: optional_f64(entry, "retry_rate_min")?,
            retry_rate_max: optional_f64(entry, "retry_rate_max")?,
            archive_corruption_min: optional_u64(entry, "archive_corruption_min")?,
            archive_corruption_max: optional_u64(entry, "archive_corruption_max")?,
            libgit2_retries_max: optional_u64(entry, "libgit2_retries_max")?,
        };
        if let (Some(min), Some(max)) = (threshold.retry_rate_min, threshold.retry_rate_max)
            && min > max
        {
            return Err(format!("[{key}] retry_rate_min exceeds retry_rate_max"));
        }
        for rate in [threshold.retry_rate_min, threshold.retry_rate_max]
            .into_iter()
            .flatten()
        {
            if !(0.0..=1.0).contains(&rate) {
                return Err(format!("[{key}] retry_rate value out of range: {rate}"));
            }
        }
        out.insert(key.to_string(), threshold);
    }
    Ok(out)
}

fn optional_f64(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<f64>, String> {
    table
        .get(key)
        .map(|v| {
            v.as_float()
                .or_else(|| v.as_integer().map(|i| i as f64))
                .ok_or_else(|| format!("{key} must be numeric"))
        })
        .transpose()
}

fn optional_u64(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<u64>, String> {
    table
        .get(key)
        .map(|v| {
            v.as_integer()
                .and_then(|i| u64::try_from(i).ok())
                .ok_or_else(|| format!("{key} must be a non-negative integer"))
        })
        .transpose()
}

fn assert_test_infrastructure() {
    assert_racer_shim_prob_0_never_injects();
    assert_racer_shim_prob_1_injects_on_target_subcommand();
    assert_racer_shim_trigger_filter_passes_unrelated_commands();
    assert!(load_thresholds().expect("valid fixture").len() == 4);
    assert!(parse_thresholds("[scenario_a]\nretry_rate_max = -0.1\n").is_err());
    assert!(
        parse_thresholds(
            r#"
[scenario_a]
retry_rate_min = 0.5
retry_rate_max = 0.1
[scenario_b]
retry_rate_min = 0.03
retry_rate_max = 0.08
[scenario_c]
retry_rate_max = 0.05
[scenario_d]
retry_rate_max = 0.05
"#
        )
        .is_err()
    );

    let events = EventSink::default();
    let stats = WorkloadStats {
        logical_git_calls: 1,
        ..WorkloadStats::default()
    };
    emit_event(
        &events,
        Scenario::A,
        "info",
        "artifact_writer_probe",
        json!({ "caller": "unit" }),
        None,
    );
    let artifact_dir = write_artifacts(Scenario::A, &stats, &events).expect("write artifacts");
    for name in [
        "metrics.json",
        "events.jsonl",
        "summary.json",
        "stdout.log",
        "stderr.log",
        "replay.sh",
    ] {
        assert!(
            artifact_dir.join(name).exists(),
            "artifact {name} should exist"
        );
    }
    let summary: Value =
        serde_json::from_slice(&fs::read(artifact_dir.join("summary.json")).expect("read summary"))
            .expect("summary json");
    assert_eq!(summary["scenario"], "scenario_a");
    assert_executable(&artifact_dir.join("replay.sh"));
}

fn assert_racer_shim_prob_0_never_injects() {
    let tmp = TempDir::new().expect("tempdir");
    seed_git_repo(tmp.path());
    let status = Command::new(shim_path())
        .env("AM_TEST_RACER_PROB", "0")
        .env("AM_TEST_RACER_TRIGGERS", "update-ref")
        .args(["-C"])
        .arg(tmp.path())
        .args(["update-ref", "-d", "refs/heads/nonexistent"])
        .status()
        .expect("run shim");
    assert!(status.success(), "PROB=0 should not inject: {status:?}");
}

fn assert_racer_shim_prob_1_injects_on_target_subcommand() {
    let tmp = TempDir::new().expect("tempdir");
    seed_git_repo(tmp.path());
    let status = Command::new(shim_path())
        .env("AM_TEST_RACER_PROB", "1")
        .env("AM_TEST_RACER_TRIGGERS", "update-ref")
        .args(["-C"])
        .arg(tmp.path())
        .args(["update-ref", "-d", "refs/heads/nonexistent"])
        .status()
        .expect("run shim");
    assert!(
        is_segfault_status(status),
        "PROB=1 should inject: {status:?}"
    );
}

fn assert_racer_shim_trigger_filter_passes_unrelated_commands() {
    let tmp = TempDir::new().expect("tempdir");
    seed_git_repo(tmp.path());
    let status = Command::new(shim_path())
        .env("AM_TEST_RACER_PROB", "1")
        .env("AM_TEST_RACER_TRIGGERS", "commit")
        .args(["-C"])
        .arg(tmp.path())
        .arg("status")
        .status()
        .expect("run shim");
    assert!(
        status.success(),
        "non-trigger command should pass through: {status:?}"
    );
}

fn seed_git_repo(path: &Path) {
    run_plain_git(path, &["init", "-q", "-b", "main"]);
    run_plain_git(path, &["config", "user.email", "stress@example.invalid"]);
    run_plain_git(path, &["config", "user.name", "stress"]);
    fs::write(path.join("README.md"), "seed\n").expect("write seed");
    run_plain_git(path, &["add", "README.md"]);
    run_plain_git(path, &["commit", "-q", "-m", "seed"]);
}

fn run_plain_git(path: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed with {status:?}");
}

fn helper_requested_for(scenario: Scenario) -> bool {
    std::env::var("AM_KNOWN_BAD_GIT_HELPER").is_ok_and(|raw| raw == scenario.key())
}

fn run_scenario_in_child<F>(scenario: Scenario, configure: F)
where
    F: FnOnce(&mut Command),
{
    let mut cmd = Command::new(std::env::current_exe().expect("current test exe"));
    cmd.env("AM_KNOWN_BAD_GIT_HELPER", scenario.key()).args([
        "--exact",
        scenario.test_name(),
        "--nocapture",
    ]);
    configure(&mut cmd);
    let output = cmd.output().expect("run child scenario");
    assert!(
        output.status.success(),
        "{} child failed\nstdout:\n{}\nstderr:\n{}",
        scenario.test_name(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn extended_gate_enabled() -> bool {
    std::env::var("AM_TEST_GIT_251").is_ok_and(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn real_git_2510_gate(scenario: Scenario) -> Option<String> {
    if !extended_gate_enabled() {
        eprintln!("{} skipped: set AM_TEST_GIT_251=1", scenario.test_name());
        return None;
    }
    let Ok(path) = std::env::var("AM_GIT_BINARY") else {
        eprintln!(
            "{} skipped: set AM_GIT_BINARY to a real git 2.51.0 binary",
            scenario.test_name()
        );
        return None;
    };
    if !Path::new(&path).is_file() {
        eprintln!(
            "{} skipped: AM_GIT_BINARY is not a file",
            scenario.test_name()
        );
        return None;
    }
    let output = Command::new(&path).arg("--version").output();
    let Ok(output) = output else {
        eprintln!(
            "{} skipped: AM_GIT_BINARY did not run",
            scenario.test_name()
        );
        return None;
    };
    let version = String::from_utf8_lossy(&output.stdout);
    if !version.contains("git version 2.51.0") {
        eprintln!(
            "{} skipped: AM_GIT_BINARY is not git 2.51.0 ({})",
            scenario.test_name(),
            version.trim()
        );
        return None;
    }
    Some(path)
}

fn emit_event(
    events: &EventSink,
    scenario: Scenario,
    level: &str,
    message: &str,
    data: Value,
    latency_ms: Option<u64>,
) {
    let mut event = json!({
        "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "test": "stress_known_bad_git",
        "scenario": scenario.key(),
        "level": level,
        "message": message,
        "data": data,
    });
    if let Some(latency_ms) = latency_ms {
        event["latency_ms"] = json!(latency_ms);
    }
    eprintln!("{event}");
    events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(event);
}

fn write_artifacts(
    scenario: Scenario,
    stats: &WorkloadStats,
    events: &EventSink,
) -> io::Result<PathBuf> {
    let dir = artifact_dir(scenario);
    fs::create_dir_all(&dir)?;
    let summary = summary_json(scenario, stats);
    fs::write(
        dir.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    fs::write(
        dir.join("metrics.json"),
        serde_json::to_vec_pretty(&json!({
            "git_segfault_total": stats.segfault_retries,
            "git_segfault_retry_attempted_total": stats.segfault_retries,
            "git_segfault_retry_succeeded_total": stats.segfault_retries.saturating_sub(stats.git_errors),
            "git_segfault_retry_exhausted_total": stats.git_errors,
            "run_git_locked_total": stats.logical_git_calls,
            "archive_corruption_count": stats.archive_corruption_count,
            "libgit2_retries": stats.libgit2_retries,
        }))?,
    )?;
    let event_lines = events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(dir.join("events.jsonl"), format!("{event_lines}\n"))?;
    fs::write(
        dir.join("stdout.log"),
        format!(
            "scenario={} messages={}\n",
            scenario.key(),
            stats.total_messages
        ),
    )?;
    fs::write(
        dir.join("stderr.log"),
        format!(
            "scenario={} retries={} retry_rate={:.4}\n",
            scenario.key(),
            stats.segfault_retries,
            stats.retry_rate()
        ),
    )?;
    let replay = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\ncargo test -p mcp-agent-mail-storage --test stress_pipeline_known_bad_git {} -- --nocapture\n",
        scenario.test_name()
    );
    let replay_path = dir.join("replay.sh");
    fs::write(&replay_path, replay)?;
    make_executable(&replay_path)?;
    Ok(dir)
}

fn summary_json(scenario: Scenario, stats: &WorkloadStats) -> Value {
    json!({
        "scenario": scenario.key(),
        "total_calls": stats.logical_git_calls,
        "retries": stats.segfault_retries,
        "segfaults": stats.segfault_retries,
        "retry_rate": stats.retry_rate(),
        "duration_ms": stats.duration_ms,
        "archive_corruption_count": stats.archive_corruption_count,
        "libgit2_retries": stats.libgit2_retries,
        "archive_errors": stats.archive_errors,
        "git_errors": stats.git_errors,
        "total_messages": stats.total_messages,
    })
}

fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let cx = Cx::for_testing();
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    rt.block_on(f(cx))
}

fn block_on_with_retry<F, Fut, T>(max_retries: usize, f: F) -> T
where
    F: Fn(Cx) -> Fut,
    Fut: std::future::Future<Output = Outcome<T, mcp_agent_mail_db::DbError>>,
{
    for attempt in 0..=max_retries {
        let cx = Cx::for_testing();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        match rt.block_on(f(cx)) {
            Outcome::Ok(val) => return val,
            Outcome::Err(error) if attempt < max_retries => {
                let msg = format!("{error:?}");
                if msg.contains("locked") || msg.contains("busy") {
                    std::thread::sleep(Duration::from_millis(10 * (attempt as u64 + 1)));
                    continue;
                }
                panic!("non-retryable DB error on attempt {attempt}: {error:?}");
            }
            Outcome::Err(error) => panic!("DB failed after {max_retries} retries: {error:?}"),
            Outcome::Cancelled(reason) => panic!("DB cancelled: {reason:?}"),
            Outcome::Panicked(panic) => panic!("DB panicked: {panic}"),
        }
    }
    unreachable!()
}

fn make_pool(tmp: &TempDir) -> DbPool {
    let db_path = tmp
        .path()
        .join(format!("known_bad_git_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        storage_root: Some(db_path.parent().unwrap().join("storage")),
        max_connections: 24,
        min_connections: 4,
        acquire_timeout_ms: 60_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    };
    DbPool::new(&config).expect("create DB pool")
}

fn test_config(root: &Path) -> Config {
    Config {
        storage_root: root.to_path_buf(),
        ..Config::default()
    }
}

fn unique_human_key(prefix: &str) -> String {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    format!("/tmp/{prefix}-{suffix}-{}", unique_suffix())
}

fn unique_suffix() -> u64 {
    UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| {
        let mut out: String = f.to_uppercase().collect();
        out.extend(c);
        out
    })
}

fn agent_name(idx: usize) -> String {
    let adj = VALID_ADJECTIVES[idx % VALID_ADJECTIVES.len()];
    let noun = VALID_NOUNS[idx % VALID_NOUNS.len()];
    format!("{}{}", cap(adj), cap(noun))
}

fn args_hash(args: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for arg in args {
        hasher.update(arg.as_bytes());
        hasher.update([0]);
    }
    hex::encode(&hasher.finalize()[..8])
}

fn duration_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn threshold_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stress_pipeline_thresholds.toml")
}

fn shim_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/git_segfault_shim.sh")
}

fn artifact_dir(scenario: Scenario) -> PathBuf {
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ");
    workspace_root()
        .join("tests/artifacts/stress/git_251")
        .join(format!("{ts}-{}", unique_suffix()))
        .join(scenario.key())
}

fn make_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(perms.mode() | 0o755);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn assert_executable(path: &Path) {
    #[cfg(unix)]
    {
        let mode = fs::metadata(path).expect("metadata").permissions().mode();
        assert_ne!(mode & 0o111, 0, "{} should be executable", path.display());
    }
}

fn is_segfault_status(status: std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .signal()
            .is_some_and(|signal| signal == 11 || signal == 7)
            || matches!(status.code(), Some(139 | 135))
    }
    #[cfg(not(unix))]
    {
        matches!(status.code().map(|code| code as u32), Some(0xC000_0005))
    }
}
