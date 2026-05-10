//! 1000-agent load simulation benchmarks (br-15dv.7.2).
//!
//! Four scenarios exercising the DB layer under realistic concurrent load:
//!
//! - **Scenario A**: Registration storm — 1000 agents register across 50 threads.
//! - **Scenario B**: Message burst — 100 agents send 10 messages each.
//! - **Scenario C**: Mixed workload — 60s sustained mixed read/write operations.
//! - **Scenario D**: Thundering herd — 500 concurrent `fetch_inbox` on one project.
//!
//! Each scenario collects per-operation latencies, reports p50/p95/p99/max,
//! and asserts SLO budgets from br-15dv.10.
//!
//! # Running
//!
//! ```sh
//! cargo test -p mcp-agent-mail-db --test load_bench -- --ignored --nocapture
//! ```

#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::needless_collect
)]

mod common;

use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::config::CacheProfile;
use mcp_agent_mail_core::models::{VALID_ADJECTIVES, VALID_NOUNS};
use mcp_agent_mail_db::AgentRow;
use mcp_agent_mail_db::cache::ReadCache;
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::{DbPool, DbPoolConfig, QUERY_TRACKER, read_cache};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    common::block_on(f)
}

fn block_on_with_retry<F, Fut, T>(max_retries: usize, f: F) -> T
where
    F: Fn(Cx) -> Fut,
    Fut: std::future::Future<Output = Outcome<T, mcp_agent_mail_db::DbError>>,
{
    for attempt in 0..=max_retries {
        match common::block_on(&f) {
            Outcome::Ok(val) => return val,
            Outcome::Err(e) if attempt < max_retries => {
                let msg = format!("{e:?}");
                if msg.contains("locked") || msg.contains("busy") {
                    std::thread::sleep(Duration::from_millis(10 * (attempt as u64 + 1)));
                    continue;
                }
                panic!("non-retryable error on attempt {attempt}: {e:?}");
            }
            Outcome::Err(e) => panic!("failed after {max_retries} retries: {e:?}"),
            _ => panic!("unexpected outcome"),
        }
    }
    unreachable!()
}

fn make_load_pool(max_connections: usize) -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join(format!("load_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        storage_root: Some(db_path.parent().unwrap().join("storage")),
        max_connections,
        min_connections: 4_usize.min(max_connections),
        acquire_timeout_ms: 120_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| {
        let mut out: String = f.to_uppercase().collect();
        out.extend(c);
        out
    })
}

fn generate_agent_names(count: usize) -> Vec<String> {
    let mut names = Vec::with_capacity(count);
    'name_gen: for adj in VALID_ADJECTIVES {
        for noun in VALID_NOUNS {
            names.push(format!("{}{}", cap(adj), cap(noun)));
            if names.len() >= count {
                break 'name_gen;
            }
        }
    }
    assert!(
        names.len() >= count,
        "need {count} unique agent names, got {}",
        names.len()
    );
    names.truncate(count);
    names
}

/// Compute percentiles from a sorted slice of microsecond latencies.
#[derive(Clone, serde::Serialize)]
struct LatencyReport {
    count: usize,
    p50: u64,
    p95: u64,
    p99: u64,
    max: u64,
    errors: u64,
}

impl LatencyReport {
    fn from_latencies(latencies: &mut [u64], errors: u64) -> Self {
        latencies.sort_unstable();
        let n = latencies.len();
        if n == 0 {
            return Self {
                count: 0,
                p50: 0,
                p95: 0,
                p99: 0,
                max: 0,
                errors,
            };
        }
        Self {
            count: n,
            p50: latencies[n * 50 / 100],
            p95: latencies[n * 95 / 100],
            p99: latencies[n * 99 / 100],
            max: latencies[n - 1],
            errors,
        }
    }

    fn print(&self, label: &str) {
        eprintln!(
            "  {label}: n={}, p50={:.1}ms, p95={:.1}ms, p99={:.1}ms, max={:.1}ms, errors={}",
            self.count,
            self.p50 as f64 / 1000.0,
            self.p95 as f64 / 1000.0,
            self.p99 as f64 / 1000.0,
            self.max as f64 / 1000.0,
            self.errors,
        );
    }
}

fn run_inbox_stats_polling_phase(
    pool: &DbPool,
    receiver_id: i64,
    polls: usize,
    force_invalidate_each_poll: bool,
) -> (LatencyReport, u64) {
    let mut latencies: Vec<u64> = Vec::with_capacity(polls);
    for _ in 0..polls {
        if force_invalidate_each_poll {
            read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), receiver_id);
        }

        let t0 = Instant::now();
        let outcome = block_on(|cx| {
            let pp = pool.clone();
            async move { queries::get_inbox_stats(&cx, &pp, receiver_id).await }
        });
        match outcome {
            Outcome::Ok(Some(_)) => {
                latencies.push(t0.elapsed().as_micros() as u64);
            }
            other => panic!("get_inbox_stats polling failed: {other:?}"),
        }
    }

    let snapshot = QUERY_TRACKER.snapshot();
    let inbox_stats_queries = snapshot.per_table.get("inbox_stats").copied().unwrap_or(0);
    (
        LatencyReport::from_latencies(&mut latencies, 0),
        inbox_stats_queries,
    )
}

#[derive(serde::Serialize)]
struct SwarmLoadLabScenario {
    name: &'static str,
    projects: usize,
    agents_per_project: usize,
    total_agents: usize,
    messages_per_agent: usize,
    default_ci: bool,
    ignored_heavy: bool,
    operations: Vec<&'static str>,
}

#[derive(serde::Serialize)]
struct SwarmLoadLabOperationReport {
    operation: &'static str,
    count: usize,
    errors: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
}

#[derive(serde::Serialize)]
struct SwarmLoadLabResourceLedger {
    baseline_rss_kb: u64,
    final_rss_kb: u64,
    rss_growth_kb: u64,
    wal_bytes: u64,
    process_cpu_ticks_delta: u64,
    db_query_count: u64,
    per_table_queries: BTreeMap<String, u64>,
    isolated_storage_root: String,
    isolated_sqlite_path: String,
}

#[derive(serde::Serialize)]
struct SwarmLoadLabGate {
    name: &'static str,
    budget: String,
    actual: String,
    passed: bool,
}

#[derive(serde::Serialize)]
struct SwarmLoadLabReport {
    bead: &'static str,
    generated_at: String,
    scenario: &'static str,
    operation_reports: Vec<SwarmLoadLabOperationReport>,
    scenario_definitions: Vec<SwarmLoadLabScenario>,
    resource_ledger: SwarmLoadLabResourceLedger,
    gates: Vec<SwarmLoadLabGate>,
    reproduction_commands: Vec<String>,
    realism_notes: Vec<&'static str>,
}

#[derive(serde::Serialize)]
struct CacheProfileHotsetReport {
    profile: &'static str,
    capacity_per_category: usize,
    seeded_agents: usize,
    probes: usize,
    hits: u64,
    misses: u64,
    hit_ratio: f64,
    lookup_p50_us: u64,
    lookup_p95_us: u64,
    lookup_p99_us: u64,
    lookup_max_us: u64,
    output_checksum: u64,
    final_live_entries: usize,
    capacity_utilization_bp: u64,
    total_estimated_bytes: usize,
}

impl SwarmLoadLabOperationReport {
    const fn from_latency_report(operation: &'static str, report: &LatencyReport) -> Self {
        Self {
            operation,
            count: report.count,
            errors: report.errors,
            p50_us: report.p50,
            p95_us: report.p95,
            p99_us: report.p99,
            max_us: report.max,
        }
    }
}

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map_or(0, |pages| pages * 4)
}

fn process_cpu_ticks() -> u64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let (_, fields) = s.rsplit_once(") ")?;
            let fields: Vec<&str> = fields.split_whitespace().collect();
            let utime = fields.get(11)?.parse::<u64>().ok()?;
            let stime = fields.get(12)?.parse::<u64>().ok()?;
            Some(utime + stime)
        })
        .unwrap_or(0)
}

fn wal_size_bytes(db_path: &str) -> u64 {
    std::fs::metadata(format!("{db_path}-wal")).map_or(0, |meta| meta.len())
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn swarm_load_lab_artifact_dir() -> PathBuf {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    repo_root().join(format!(
        "tests/artifacts/perf/swarm_load_lab/{ts}_{}",
        std::process::id()
    ))
}

fn markdown_for_swarm_load_lab(report: &SwarmLoadLabReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Swarm Load Lab Report");
    let _ = writeln!(out);
    let _ = writeln!(out, "- Bead: `{}`", report.bead);
    let _ = writeln!(out, "- Scenario: `{}`", report.scenario);
    let _ = writeln!(out, "- Generated: `{}`", report.generated_at);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Operation Latency");
    let _ = writeln!(
        out,
        "| Operation | Count | Errors | p50 | p95 | p99 | Max |"
    );
    let _ = writeln!(out, "|---|---:|---:|---:|---:|---:|---:|");
    for op in &report.operation_reports {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {}us | {}us | {}us | {}us |",
            op.operation, op.count, op.errors, op.p50_us, op.p95_us, op.p99_us, op.max_us
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Resource Ledger");
    let _ = writeln!(
        out,
        "- RSS growth: `{}` KiB",
        report.resource_ledger.rss_growth_kb
    );
    let _ = writeln!(out, "- WAL bytes: `{}`", report.resource_ledger.wal_bytes);
    let _ = writeln!(
        out,
        "- CPU ticks delta: `{}`",
        report.resource_ledger.process_cpu_ticks_delta
    );
    let _ = writeln!(
        out,
        "- DB query count: `{}`",
        report.resource_ledger.db_query_count
    );
    let _ = writeln!(
        out,
        "- Isolated SQLite path: `{}`",
        report.resource_ledger.isolated_sqlite_path
    );
    let _ = writeln!(
        out,
        "- Isolated storage root: `{}`",
        report.resource_ledger.isolated_storage_root
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "## Gates");
    let _ = writeln!(out, "| Gate | Budget | Actual | Verdict |");
    let _ = writeln!(out, "|---|---:|---:|---|");
    for gate in &report.gates {
        let verdict = if gate.passed { "PASS" } else { "FAIL" };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            gate.name, gate.budget, gate.actual, verdict
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Reproduction");
    for command in &report.reproduction_commands {
        let _ = writeln!(out, "- `{command}`");
    }
    out
}

fn write_swarm_load_lab_artifacts(report: &SwarmLoadLabReport) {
    let dir = swarm_load_lab_artifact_dir();
    std::fs::create_dir_all(&dir).expect("create swarm load lab artifact dir");
    let json_path = dir.join("report.json");
    let markdown_path = dir.join("report.md");
    let json = serde_json::to_string_pretty(report).expect("serialize swarm load lab report");
    std::fs::write(&json_path, json).expect("write swarm load lab json report");
    std::fs::write(&markdown_path, markdown_for_swarm_load_lab(report))
        .expect("write swarm load lab markdown report");
    eprintln!("swarm load lab json artifact: {}", json_path.display());
    eprintln!(
        "swarm load lab markdown artifact: {}",
        markdown_path.display()
    );
}

fn cache_profile_hotset_artifact_dir() -> PathBuf {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    repo_root().join(format!(
        "tests/artifacts/perf/cache_profile_hotset/{ts}_{}",
        std::process::id()
    ))
}

fn write_cache_profile_hotset_artifact(report: &serde_json::Value) {
    let dir = cache_profile_hotset_artifact_dir();
    std::fs::create_dir_all(&dir).expect("create cache profile hotset artifact dir");
    let json_path = dir.join("report.json");
    let json = serde_json::to_string_pretty(report).expect("serialize cache profile hotset report");
    std::fs::write(&json_path, json).expect("write cache profile hotset json report");
    eprintln!(
        "cache profile hotset json artifact: {}",
        json_path.display()
    );
}

fn build_swarm_load_lab_gates(
    operation_reports: &[SwarmLoadLabOperationReport],
    resource_ledger: &SwarmLoadLabResourceLedger,
) -> Vec<SwarmLoadLabGate> {
    let max_p95 = operation_reports
        .iter()
        .map(|report| report.p95_us)
        .max()
        .unwrap_or(0);
    let max_p99 = operation_reports
        .iter()
        .map(|report| report.p99_us)
        .max()
        .unwrap_or(0);
    let total_errors: u64 = operation_reports.iter().map(|report| report.errors).sum();

    vec![
        SwarmLoadLabGate {
            name: "operation_errors",
            budget: "0".to_string(),
            actual: total_errors.to_string(),
            passed: total_errors == 0,
        },
        SwarmLoadLabGate {
            name: "max_operation_p95_us",
            budget: "1_000_000".to_string(),
            actual: max_p95.to_string(),
            passed: max_p95 <= 1_000_000,
        },
        SwarmLoadLabGate {
            name: "max_operation_p99_us",
            budget: "3_000_000".to_string(),
            actual: max_p99.to_string(),
            passed: max_p99 <= 3_000_000,
        },
        SwarmLoadLabGate {
            name: "rss_growth_kb",
            budget: "204_800".to_string(),
            actual: resource_ledger.rss_growth_kb.to_string(),
            passed: resource_ledger.rss_growth_kb <= 204_800,
        },
        SwarmLoadLabGate {
            name: "wal_bytes",
            budget: "134_217_728".to_string(),
            actual: resource_ledger.wal_bytes.to_string(),
            passed: resource_ledger.wal_bytes <= 134_217_728,
        },
        SwarmLoadLabGate {
            name: "db_queries_present",
            budget: ">0".to_string(),
            actual: resource_ledger.db_query_count.to_string(),
            passed: resource_ledger.db_query_count > 0,
        },
    ]
}

fn swarm_load_lab_scenario_definitions() -> Vec<SwarmLoadLabScenario> {
    vec![
        SwarmLoadLabScenario {
            name: "ci_smoke",
            projects: 3,
            agents_per_project: 4,
            total_agents: 12,
            messages_per_agent: 2,
            default_ci: true,
            ignored_heavy: false,
            operations: vec![
                "ensure_project",
                "register_agent",
                "ensure_product",
                "products_link",
                "send_message",
                "fetch_inbox",
                "search_messages",
                "file_reservation_paths",
                "robot_status_snapshot_surrogate",
                "startup_integrity_check",
            ],
        },
        SwarmLoadLabScenario {
            name: "ignored_1k_registration_storm",
            projects: 50,
            agents_per_project: 20,
            total_agents: 1000,
            messages_per_agent: 0,
            default_ci: false,
            ignored_heavy: true,
            operations: vec!["ensure_project", "register_agent"],
        },
        SwarmLoadLabScenario {
            name: "ignored_1k_mixed_workload",
            projects: 50,
            agents_per_project: 20,
            total_agents: 1000,
            messages_per_agent: 0,
            default_ci: false,
            ignored_heavy: true,
            operations: vec![
                "fetch_inbox",
                "send_message",
                "search_messages",
                "file_reservation_paths",
                "acknowledge_message",
            ],
        },
    ]
}

// ---------------------------------------------------------------------------
// CI-safe swarm load lab smoke
// ---------------------------------------------------------------------------

#[test]
fn swarm_load_lab_ci_smoke_writes_slo_artifacts() {
    let (pool, _dir) = make_load_pool(24);
    let sqlite_path = pool.sqlite_path().to_string();
    let storage_root = Path::new(&sqlite_path)
        .parent()
        .expect("sqlite path parent")
        .join("storage");
    let names = generate_agent_names(12);
    let baseline_rss = rss_kb();
    let baseline_cpu = process_cpu_ticks();

    QUERY_TRACKER.enable(None);
    QUERY_TRACKER.reset();

    let mut register_lats = Vec::new();
    let mut product_lats = Vec::new();
    let mut send_lats = Vec::new();
    let mut inbox_lats = Vec::new();
    let mut search_lats = Vec::new();
    let mut reservation_lats = Vec::new();
    let mut robot_snapshot_lats = Vec::new();
    let mut recovery_lats = Vec::new();
    let mut register_errors = 0_u64;
    let mut product_errors = 0_u64;
    let mut send_errors = 0_u64;
    let mut inbox_errors = 0_u64;
    let mut search_errors = 0_u64;
    let mut reservation_errors = 0_u64;
    let mut robot_snapshot_errors = 0_u64;
    let mut recovery_errors = 0_u64;

    let mut project_data: Vec<(i64, Vec<i64>)> = Vec::new();
    for project_idx in 0..3 {
        let project_start = Instant::now();
        let project_id = block_on_with_retry(5, |cx| {
            let pp = pool.clone();
            let key = format!("/data/swarm-load-lab/ci/project-{project_idx}");
            async move { queries::ensure_project(&cx, &pp, &key).await }
        })
        .id
        .expect("project id");
        register_lats.push(project_start.elapsed().as_micros() as u64);

        let mut agent_ids = Vec::new();
        for agent_idx in 0..4 {
            let name = names[project_idx * 4 + agent_idx].clone();
            let t0 = Instant::now();
            match block_on(|cx| {
                let pp = pool.clone();
                async move {
                    queries::register_agent(
                        &cx,
                        &pp,
                        project_id,
                        &name,
                        "swarm-load-lab",
                        "ci-smoke",
                        Some("br-72syp CI smoke"),
                        None,
                        None,
                    )
                    .await
                }
            }) {
                Outcome::Ok(agent) => {
                    register_lats.push(t0.elapsed().as_micros() as u64);
                    agent_ids.push(agent.id.expect("agent id"));
                }
                _ => register_errors += 1,
            }
        }
        project_data.push((project_id, agent_ids));
    }

    let t0 = Instant::now();
    let product_id = if let Outcome::Ok(product) = block_on(|cx| {
        let pp = pool.clone();
        async move {
            queries::ensure_product(
                &cx,
                &pp,
                Some("swarm-load-lab-ci"),
                Some("Swarm Load Lab CI"),
            )
            .await
        }
    }) {
        product_lats.push(t0.elapsed().as_micros() as u64);
        product.id.expect("product id")
    } else {
        product_errors += 1;
        -1
    };
    if product_id > 0 {
        let project_ids: Vec<i64> = project_data.iter().map(|(id, _)| *id).collect();
        let t0 = Instant::now();
        match block_on(|cx| {
            let pp = pool.clone();
            async move { queries::link_product_to_projects(&cx, &pp, product_id, &project_ids).await }
        }) {
            Outcome::Ok(_) => product_lats.push(t0.elapsed().as_micros() as u64),
            _ => product_errors += 1,
        }
    }

    for (project_id, agent_ids) in &project_data {
        for (agent_idx, sender_id) in agent_ids.iter().copied().enumerate() {
            for msg_idx in 0..2 {
                let receiver = agent_ids[(agent_idx + msg_idx + 1) % agent_ids.len()];
                let t0 = Instant::now();
                match block_on(|cx| {
                    let pp = pool.clone();
                    async move {
                        queries::create_message_with_recipients(
                            &cx,
                            &pp,
                            *project_id,
                            sender_id,
                            &format!("swarm smoke {project_id}-{agent_idx}-{msg_idx}"),
                            "swarm load lab smoke body for inbox and search paths",
                            Some("br-72syp-ci-smoke"),
                            "normal",
                            msg_idx == 0,
                            "",
                            &[(receiver, "to")],
                        )
                        .await
                    }
                }) {
                    Outcome::Ok(_) => send_lats.push(t0.elapsed().as_micros() as u64),
                    _ => send_errors += 1,
                }
            }
        }
    }

    for (project_id, agent_ids) in &project_data {
        for agent_id in agent_ids {
            let t0 = Instant::now();
            match block_on(|cx| {
                let pp = pool.clone();
                async move {
                    queries::fetch_inbox(&cx, &pp, *project_id, *agent_id, false, None, 20).await
                }
            }) {
                Outcome::Ok(_) => inbox_lats.push(t0.elapsed().as_micros() as u64),
                _ => inbox_errors += 1,
            }

            let t0 = Instant::now();
            match block_on(|cx| {
                let pp = pool.clone();
                async move { queries::get_inbox_stats(&cx, &pp, *agent_id).await }
            }) {
                Outcome::Ok(_) => robot_snapshot_lats.push(t0.elapsed().as_micros() as u64),
                _ => robot_snapshot_errors += 1,
            }
        }
    }

    for (project_id, agent_ids) in &project_data {
        let t0 = Instant::now();
        match block_on(|cx| {
            let pp = pool.clone();
            async move { queries::search_messages(&cx, &pp, *project_id, "swarm", 20).await }
        }) {
            Outcome::Ok(_) => search_lats.push(t0.elapsed().as_micros() as u64),
            _ => search_errors += 1,
        }

        for (idx, agent_id) in agent_ids.iter().copied().take(2).enumerate() {
            let t0 = Instant::now();
            let path = format!("src/swarm_lab/project_{project_id}/agent_{idx}.rs");
            match block_on(|cx| {
                let pp = pool.clone();
                async move {
                    queries::create_file_reservations(
                        &cx,
                        &pp,
                        *project_id,
                        agent_id,
                        &[path.as_str()],
                        300,
                        true,
                        "br-72syp ci smoke",
                    )
                    .await
                }
            }) {
                Outcome::Ok(_) => reservation_lats.push(t0.elapsed().as_micros() as u64),
                _ => reservation_errors += 1,
            }
        }
    }

    let t0 = Instant::now();
    match pool.run_startup_integrity_check() {
        Ok(_) => recovery_lats.push(t0.elapsed().as_micros() as u64),
        Err(_) => recovery_errors += 1,
    }

    let tracker_snapshot = QUERY_TRACKER.snapshot();
    QUERY_TRACKER.disable();
    QUERY_TRACKER.reset();

    let final_rss = rss_kb();
    let final_cpu = process_cpu_ticks();
    let per_table_queries: BTreeMap<String, u64> = tracker_snapshot.per_table.into_iter().collect();
    let db_query_count = per_table_queries.values().sum();

    let register_report = LatencyReport::from_latencies(&mut register_lats, register_errors);
    let product_report = LatencyReport::from_latencies(&mut product_lats, product_errors);
    let send_report = LatencyReport::from_latencies(&mut send_lats, send_errors);
    let inbox_report = LatencyReport::from_latencies(&mut inbox_lats, inbox_errors);
    let search_report = LatencyReport::from_latencies(&mut search_lats, search_errors);
    let reservation_report =
        LatencyReport::from_latencies(&mut reservation_lats, reservation_errors);
    let robot_snapshot_report =
        LatencyReport::from_latencies(&mut robot_snapshot_lats, robot_snapshot_errors);
    let recovery_report = LatencyReport::from_latencies(&mut recovery_lats, recovery_errors);
    register_report.print("load_lab_register_agent");
    product_report.print("load_lab_product_bus");
    send_report.print("load_lab_send_message");
    inbox_report.print("load_lab_fetch_inbox");
    search_report.print("load_lab_search_messages");
    reservation_report.print("load_lab_file_reservations");
    robot_snapshot_report.print("load_lab_robot_status_snapshot_surrogate");
    recovery_report.print("load_lab_startup_integrity_check");

    let operation_reports = vec![
        SwarmLoadLabOperationReport::from_latency_report("register_agent", &register_report),
        SwarmLoadLabOperationReport::from_latency_report("product_bus", &product_report),
        SwarmLoadLabOperationReport::from_latency_report("send_message", &send_report),
        SwarmLoadLabOperationReport::from_latency_report("fetch_inbox", &inbox_report),
        SwarmLoadLabOperationReport::from_latency_report("search_messages", &search_report),
        SwarmLoadLabOperationReport::from_latency_report(
            "file_reservation_paths",
            &reservation_report,
        ),
        SwarmLoadLabOperationReport::from_latency_report(
            "robot_status_snapshot_surrogate",
            &robot_snapshot_report,
        ),
        SwarmLoadLabOperationReport::from_latency_report(
            "startup_integrity_check",
            &recovery_report,
        ),
    ];
    let resource_ledger = SwarmLoadLabResourceLedger {
        baseline_rss_kb: baseline_rss,
        final_rss_kb: final_rss,
        rss_growth_kb: final_rss.saturating_sub(baseline_rss),
        wal_bytes: wal_size_bytes(&sqlite_path),
        process_cpu_ticks_delta: final_cpu.saturating_sub(baseline_cpu),
        db_query_count,
        per_table_queries,
        isolated_storage_root: storage_root.display().to_string(),
        isolated_sqlite_path: sqlite_path,
    };
    let gates = build_swarm_load_lab_gates(&operation_reports, &resource_ledger);
    let report = SwarmLoadLabReport {
        bead: "br-72syp",
        generated_at: chrono::Utc::now().to_rfc3339(),
        scenario: "ci_smoke",
        operation_reports,
        scenario_definitions: swarm_load_lab_scenario_definitions(),
        resource_ledger,
        gates,
        reproduction_commands: vec![
            "rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_mcp_agent_mail_swarm_lab cargo test -p mcp-agent-mail-db --test load_bench swarm_load_lab_ci_smoke_writes_slo_artifacts -- --nocapture".to_string(),
            "rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_mcp_agent_mail_swarm_lab cargo test -p mcp-agent-mail-db --test load_bench load_scenario_a_registration_storm -- --ignored --nocapture".to_string(),
            "rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_mcp_agent_mail_swarm_lab cargo test -p mcp-agent-mail-db --test load_bench load_scenario_c_mixed_workload -- --ignored --nocapture".to_string(),
        ],
        realism_notes: vec![
            "CI smoke uses the real DB query layer with isolated SQLite and storage roots; it does not touch the operator mailbox.",
            "The robot status lane is represented by the inbox-stats snapshot path used by robot status summaries, not by a live CLI transport process.",
            "The ignored 1k scenarios are the heavy-capacity lanes and must run through rch on suitable workers.",
        ],
    };
    write_swarm_load_lab_artifacts(&report);

    let failed_gates: Vec<&SwarmLoadLabGate> =
        report.gates.iter().filter(|gate| !gate.passed).collect();
    assert!(
        failed_gates.is_empty(),
        "swarm load lab gates failed: {}",
        failed_gates
            .iter()
            .map(|gate| gate.name)
            .collect::<Vec<_>>()
            .join(", ")
    );
}

// ---------------------------------------------------------------------------
// Scenario A: Registration storm
// ---------------------------------------------------------------------------
// 1000 agents register across 50 concurrent threads (20 agents per thread).
// Budget: p95 < 50ms per registration, 0 failures.

#[test]
#[ignore = "heavy load bench: 1000-agent registration storm"]
fn load_scenario_a_registration_storm() {
    let (pool, _dir) = make_load_pool(100);
    let names = generate_agent_names(1000);
    let n_threads: usize = 50;
    let agents_per_thread: usize = 20;
    let barrier = Arc::new(Barrier::new(n_threads));

    let start = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let chunk: Vec<String> =
                names[t * agents_per_thread..(t + 1) * agents_per_thread].to_vec();

            std::thread::spawn(move || {
                let mut latencies = Vec::with_capacity(agents_per_thread);
                let mut errors: u64 = 0;

                // Ensure project first
                let human_key = format!("/data/load/reg_p{t}_{}", unique_suffix());
                let project_id = block_on_with_retry(5, |cx| {
                    let pp = pool.clone();
                    let k = human_key.clone();
                    async move { queries::ensure_project(&cx, &pp, &k).await }
                })
                .id
                .unwrap();

                barrier.wait();

                for name in &chunk {
                    let t0 = Instant::now();
                    match block_on(|cx| {
                        let pp = pool.clone();
                        let n = name.clone();
                        async move {
                            queries::register_agent(
                                &cx,
                                &pp,
                                project_id,
                                &n,
                                "load-bench",
                                "model",
                                None,
                                None,
                                None,
                            )
                            .await
                        }
                    }) {
                        Outcome::Ok(_) => {
                            latencies.push(t0.elapsed().as_micros() as u64);
                        }
                        _ => errors += 1,
                    }
                }
                (latencies, errors)
            })
        })
        .collect();

    let mut all_latencies = Vec::with_capacity(1000);
    let mut total_errors: u64 = 0;
    for h in handles {
        let (lats, errs) = h.join().expect("thread should not panic");
        all_latencies.extend(lats);
        total_errors += errs;
    }

    let elapsed = start.elapsed();
    let report = LatencyReport::from_latencies(&mut all_latencies, total_errors);

    eprintln!("\n=== Scenario A: Registration Storm ===");
    eprintln!("  Total time: {:.2}s", elapsed.as_secs_f64());
    report.print("register_agent");
    eprintln!(
        "  Throughput: {:.0} registrations/s",
        report.count as f64 / elapsed.as_secs_f64()
    );

    assert_eq!(total_errors, 0, "expected 0 errors, got {total_errors}");
    assert_eq!(report.count, 1000, "expected 1000 registrations");
    assert!(
        report.p95 < 50_000,
        "SLO: p95 < 50ms, got {:.1}ms",
        report.p95 as f64 / 1000.0
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "expected < 10s, took {:.1}s",
        elapsed.as_secs_f64()
    );
}

// ---------------------------------------------------------------------------
// Scenario B: Message burst
// ---------------------------------------------------------------------------
// 100 agents send 10 messages each simultaneously (20 threads × 50 messages).
// Budget: p95 < 100ms per send, p99 < 500ms, 0 lost messages.

#[test]
#[ignore = "heavy load bench: 100-agent message burst"]
fn load_scenario_b_message_burst() {
    let (pool, _dir) = make_load_pool(100);
    let names = generate_agent_names(100);
    let n_agents: usize = 100;
    let msgs_per_agent: usize = 10;
    let n_threads: usize = 20;
    let agents_per_thread: usize = n_agents / n_threads;

    // Setup: create one project and register all agents
    let project_id = block_on_with_retry(5, |cx| {
        let pp = pool.clone();
        let k = format!("/data/load/burst_{}", unique_suffix());
        async move { queries::ensure_project(&cx, &pp, &k).await }
    })
    .id
    .unwrap();

    let mut agent_ids: Vec<i64> = Vec::with_capacity(n_agents);
    for name in &names {
        let aid = block_on_with_retry(5, |cx| {
            let pp = pool.clone();
            let n = name.clone();
            async move {
                queries::register_agent(
                    &cx,
                    &pp,
                    project_id,
                    &n,
                    "load-bench",
                    "model",
                    None,
                    None,
                    None,
                )
                .await
            }
        })
        .id
        .unwrap();
        agent_ids.push(aid);
    }

    let agent_ids = Arc::new(agent_ids);
    let barrier = Arc::new(Barrier::new(n_threads));
    let start = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let agent_ids = Arc::clone(&agent_ids);
            let start_idx = t * agents_per_thread;

            std::thread::spawn(move || {
                let mut latencies = Vec::with_capacity(agents_per_thread * msgs_per_agent);
                let mut errors: u64 = 0;

                barrier.wait();

                for a in start_idx..start_idx + agents_per_thread {
                    let sender_id = agent_ids[a];
                    for m in 0..msgs_per_agent {
                        let receiver_idx = (a + m + 1) % n_agents;
                        let receiver_id = agent_ids[receiver_idx];

                        let t0 = Instant::now();
                        match block_on(|cx| {
                            let pp = pool.clone();
                            async move {
                                queries::create_message_with_recipients(
                                    &cx,
                                    &pp,
                                    project_id,
                                    sender_id,
                                    &format!("burst-a{a}-m{m}"),
                                    &format!("body {a}-{m}"),
                                    None,
                                    "normal",
                                    false,
                                    "",
                                    &[(receiver_id, "to")],
                                )
                                .await
                            }
                        }) {
                            Outcome::Ok(_) => {
                                latencies.push(t0.elapsed().as_micros() as u64);
                            }
                            _ => errors += 1,
                        }
                    }
                }
                (latencies, errors)
            })
        })
        .collect();

    let mut all_latencies = Vec::with_capacity(n_agents * msgs_per_agent);
    let mut total_errors: u64 = 0;
    for h in handles {
        let (lats, errs) = h.join().expect("thread should not panic");
        all_latencies.extend(lats);
        total_errors += errs;
    }

    let elapsed = start.elapsed();
    let report = LatencyReport::from_latencies(&mut all_latencies, total_errors);

    eprintln!("\n=== Scenario B: Message Burst ===");
    eprintln!("  Total time: {:.2}s", elapsed.as_secs_f64());
    report.print("send_message");
    eprintln!(
        "  Throughput: {:.0} messages/s",
        report.count as f64 / elapsed.as_secs_f64()
    );

    assert_eq!(total_errors, 0, "expected 0 errors, got {total_errors}");
    assert_eq!(
        report.count,
        n_agents * msgs_per_agent,
        "expected {} messages",
        n_agents * msgs_per_agent
    );
    assert!(
        report.p95 < 100_000,
        "SLO: p95 < 100ms, got {:.1}ms",
        report.p95 as f64 / 1000.0
    );
    assert!(
        report.p99 < 500_000,
        "SLO: p99 < 500ms, got {:.1}ms",
        report.p99 as f64 / 1000.0
    );
}

// ---------------------------------------------------------------------------
// Scenario C: Mixed workload
// ---------------------------------------------------------------------------
// 1000 agents across 50 projects cycle through mixed operations for 30 seconds.
// Operation mix: 40% fetch_inbox, 30% send_message, 15% search,
//                10% file_reservations, 5% acknowledge.
// Budget: p95 < 200ms, p99 < 1s, 0 errors.

#[test]
#[ignore = "heavy load bench: 30s sustained mixed workload"]
fn load_scenario_c_mixed_workload() {
    let (pool, _dir) = make_load_pool(100);
    let names = generate_agent_names(1000);

    let n_projects: usize = 50;
    let agents_per_project: usize = 20;
    let n_threads: usize = 50;
    let duration = Duration::from_secs(30);

    // Setup: create projects and register agents
    let mut project_data: Vec<(i64, Vec<i64>)> = Vec::with_capacity(n_projects);
    for p in 0..n_projects {
        let project_id = block_on_with_retry(5, |cx| {
            let pp = pool.clone();
            let k = format!("/data/load/mixed_p{p}_{}", unique_suffix());
            async move { queries::ensure_project(&cx, &pp, &k).await }
        })
        .id
        .unwrap();

        let mut agent_ids = Vec::with_capacity(agents_per_project);
        for a in 0..agents_per_project {
            let name = &names[p * agents_per_project + a];
            let aid = block_on_with_retry(5, |cx| {
                let pp = pool.clone();
                let n = name.clone();
                async move {
                    queries::register_agent(
                        &cx,
                        &pp,
                        project_id,
                        &n,
                        "load-bench",
                        "model",
                        None,
                        None,
                        None,
                    )
                    .await
                }
            })
            .id
            .unwrap();
            agent_ids.push(aid);
        }
        project_data.push((project_id, agent_ids));
    }

    // Seed some messages for fetch/search/ack operations
    for (project_id, agent_ids) in &project_data {
        for a in 0..agent_ids.len().min(5) {
            let sender = agent_ids[a];
            let receiver = agent_ids[(a + 1) % agent_ids.len()];
            let _ = block_on(|cx| {
                let pp = pool.clone();
                let pid = *project_id;
                async move {
                    queries::create_message_with_recipients(
                        &cx,
                        &pp,
                        pid,
                        sender,
                        &format!("seed-{a}"),
                        "seed body",
                        None,
                        "normal",
                        true,
                        "",
                        &[(receiver, "to")],
                    )
                    .await
                }
            });
        }
    }

    let project_data = Arc::new(project_data);
    let barrier = Arc::new(Barrier::new(n_threads));

    let start = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let project_data = Arc::clone(&project_data);

            std::thread::spawn(move || {
                let mut fetch_lats = Vec::new();
                let mut send_lats = Vec::new();
                let mut search_lats = Vec::new();
                let mut reserve_lats = Vec::new();
                let mut ack_lats = Vec::new();
                let mut errors: u64 = 0;
                let mut op_counter: u64 = 0;

                barrier.wait();

                let (project_id, agent_ids) = &project_data[t % n_projects];
                let agent_id = agent_ids[t % agent_ids.len()];
                let project_id = *project_id;

                while start.elapsed() < duration {
                    // Deterministic operation selection based on counter
                    let op = op_counter % 20;
                    op_counter += 1;

                    match op {
                        // 40% fetch_inbox (0-7)
                        0..=7 => {
                            let t0 = Instant::now();
                            match block_on(|cx| {
                                let pp = pool.clone();
                                async move {
                                    queries::fetch_inbox(
                                        &cx, &pp, project_id, agent_id, false, None, 20,
                                    )
                                    .await
                                }
                            }) {
                                Outcome::Ok(_) => {
                                    fetch_lats.push(t0.elapsed().as_micros() as u64);
                                }
                                _ => errors += 1,
                            }
                        }
                        // 30% send_message (8-13)
                        8..=13 => {
                            let receiver = agent_ids[(t + op_counter as usize) % agent_ids.len()];
                            let t0 = Instant::now();
                            match block_on(|cx| {
                                let pp = pool.clone();
                                let sub = format!("mixed-t{t}-{op_counter}");
                                async move {
                                    queries::create_message_with_recipients(
                                        &cx,
                                        &pp,
                                        project_id,
                                        agent_id,
                                        &sub,
                                        "mixed workload body",
                                        None,
                                        "normal",
                                        false,
                                        "",
                                        &[(receiver, "to")],
                                    )
                                    .await
                                }
                            }) {
                                Outcome::Ok(_) => {
                                    send_lats.push(t0.elapsed().as_micros() as u64);
                                }
                                _ => errors += 1,
                            }
                        }
                        // 15% search_messages (14-16)
                        14..=16 => {
                            let t0 = Instant::now();
                            match block_on(|cx| {
                                let pp = pool.clone();
                                async move {
                                    queries::search_messages(&cx, &pp, project_id, "seed", 10).await
                                }
                            }) {
                                Outcome::Ok(_) => {
                                    search_lats.push(t0.elapsed().as_micros() as u64);
                                }
                                _ => errors += 1,
                            }
                        }
                        // 10% file_reservations (17-18)
                        17..=18 => {
                            let t0 = Instant::now();
                            match block_on(|cx| {
                                let pp = pool.clone();
                                let pat = format!("src/file_{op_counter}.rs");
                                async move {
                                    queries::create_file_reservations(
                                        &cx,
                                        &pp,
                                        project_id,
                                        agent_id,
                                        &[pat.as_str()],
                                        3600,
                                        true,
                                        "",
                                    )
                                    .await
                                }
                            }) {
                                Outcome::Ok(_) => {
                                    reserve_lats.push(t0.elapsed().as_micros() as u64);
                                }
                                _ => errors += 1,
                            }
                        }
                        // 5% acknowledge (19)
                        _ => {
                            // Fetch inbox first to find a message to ack
                            if let Outcome::Ok(msgs) = block_on(|cx| {
                                let pp = pool.clone();
                                async move {
                                    queries::fetch_inbox(
                                        &cx, &pp, project_id, agent_id, false, None, 1,
                                    )
                                    .await
                                }
                            }) && let Some(msg) = msgs.first()
                            {
                                let mid = msg.message.id.unwrap();
                                let t0 = Instant::now();
                                match block_on(|cx| {
                                    let pp = pool.clone();
                                    async move {
                                        queries::acknowledge_message(&cx, &pp, agent_id, mid).await
                                    }
                                }) {
                                    Outcome::Ok(_) => {
                                        ack_lats.push(t0.elapsed().as_micros() as u64);
                                    }
                                    _ => errors += 1,
                                }
                            }
                        }
                    }
                }
                (
                    fetch_lats,
                    send_lats,
                    search_lats,
                    reserve_lats,
                    ack_lats,
                    errors,
                )
            })
        })
        .collect();

    let mut all_fetch = Vec::new();
    let mut all_send = Vec::new();
    let mut all_search = Vec::new();
    let mut all_reserve = Vec::new();
    let mut all_ack = Vec::new();
    let mut total_errors: u64 = 0;

    for h in handles {
        let (fetch, send, search, reserve, ack, errs) = h.join().expect("thread should not panic");
        all_fetch.extend(fetch);
        all_send.extend(send);
        all_search.extend(search);
        all_reserve.extend(reserve);
        all_ack.extend(ack);
        total_errors += errs;
    }

    let elapsed = start.elapsed();
    let total_ops =
        all_fetch.len() + all_send.len() + all_search.len() + all_reserve.len() + all_ack.len();

    let fetch_r = LatencyReport::from_latencies(&mut all_fetch, 0);
    let send_r = LatencyReport::from_latencies(&mut all_send, 0);
    let search_r = LatencyReport::from_latencies(&mut all_search, 0);
    let reserve_r = LatencyReport::from_latencies(&mut all_reserve, 0);
    let ack_r = LatencyReport::from_latencies(&mut all_ack, 0);

    // Compute combined p95/p99
    let mut combined: Vec<u64> = Vec::with_capacity(total_ops);
    combined.extend(&all_fetch);
    combined.extend(&all_send);
    combined.extend(&all_search);
    combined.extend(&all_reserve);
    combined.extend(&all_ack);
    let combined_r = LatencyReport::from_latencies(&mut combined, total_errors);

    eprintln!("\n=== Scenario C: Mixed Workload (30s sustained) ===");
    eprintln!("  Duration: {:.1}s", elapsed.as_secs_f64());
    eprintln!("  Total ops: {total_ops}");
    eprintln!(
        "  Throughput: {:.0} ops/s",
        total_ops as f64 / elapsed.as_secs_f64()
    );
    fetch_r.print("fetch_inbox (40%)");
    send_r.print("send_message (30%)");
    search_r.print("search_messages (15%)");
    reserve_r.print("file_reservation (10%)");
    ack_r.print("acknowledge (5%)");
    combined_r.print("COMBINED");

    assert_eq!(total_errors, 0, "expected 0 errors, got {total_errors}");
    assert!(
        combined_r.p95 < 200_000,
        "SLO: combined p95 < 200ms, got {:.1}ms",
        combined_r.p95 as f64 / 1000.0
    );
    assert!(
        combined_r.p99 < 1_000_000,
        "SLO: combined p99 < 1s, got {:.1}ms",
        combined_r.p99 as f64 / 1000.0
    );
}

// ---------------------------------------------------------------------------
// Scenario D: Thundering herd
// ---------------------------------------------------------------------------
// 500 concurrent threads all call `fetch_inbox` on the same project at once.
// Budget: p95 < 500ms, 0 errors.

#[test]
#[ignore = "heavy load bench: 500-thread thundering herd"]
fn load_scenario_d_thundering_herd() {
    let (pool, _dir) = make_load_pool(100);

    // Setup: one project with 500 agents and some seeded messages
    let project_id = block_on_with_retry(5, |cx| {
        let pp = pool.clone();
        let k = format!("/data/load/herd_{}", unique_suffix());
        async move { queries::ensure_project(&cx, &pp, &k).await }
    })
    .id
    .unwrap();

    let names = generate_agent_names(500);
    let mut agent_ids: Vec<i64> = Vec::with_capacity(500);
    for name in &names {
        let aid = block_on_with_retry(5, |cx| {
            let pp = pool.clone();
            let n = name.clone();
            async move {
                queries::register_agent(
                    &cx,
                    &pp,
                    project_id,
                    &n,
                    "load-bench",
                    "model",
                    None,
                    None,
                    None,
                )
                .await
            }
        })
        .id
        .unwrap();
        agent_ids.push(aid);
    }

    // Seed 50 messages so inboxes aren't trivially empty
    for i in 0..50 {
        let sender = agent_ids[i % agent_ids.len()];
        let receiver = agent_ids[(i + 1) % agent_ids.len()];
        let _ = block_on(|cx| {
            let pp = pool.clone();
            async move {
                queries::create_message_with_recipients(
                    &cx,
                    &pp,
                    project_id,
                    sender,
                    &format!("herd-seed-{i}"),
                    "herd seed body",
                    None,
                    "normal",
                    false,
                    "",
                    &[(receiver, "to")],
                )
                .await
            }
        });
    }

    let n_threads: usize = 500;
    let agent_ids = Arc::new(agent_ids);
    let barrier = Arc::new(Barrier::new(n_threads));

    let start = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let agent_ids = Arc::clone(&agent_ids);

            std::thread::spawn(move || {
                let agent_id = agent_ids[t];

                barrier.wait();

                let t0 = Instant::now();
                let result = block_on(|cx| {
                    let pp = pool.clone();
                    async move {
                        queries::fetch_inbox(&cx, &pp, project_id, agent_id, false, None, 20).await
                    }
                });

                let latency = t0.elapsed().as_micros() as u64;
                let error = !matches!(result, Outcome::Ok(_));
                (latency, error)
            })
        })
        .collect();

    let mut latencies = Vec::with_capacity(n_threads);
    let mut total_errors: u64 = 0;
    for h in handles {
        let (lat, err) = h.join().expect("thread should not panic");
        latencies.push(lat);
        if err {
            total_errors += 1;
        }
    }

    let elapsed = start.elapsed();
    let report = LatencyReport::from_latencies(&mut latencies, total_errors);

    eprintln!("\n=== Scenario D: Thundering Herd (500 concurrent) ===");
    eprintln!("  Total time: {:.2}s", elapsed.as_secs_f64());
    report.print("fetch_inbox");
    eprintln!(
        "  Throughput: {:.0} ops/s",
        report.count as f64 / elapsed.as_secs_f64()
    );

    assert_eq!(total_errors, 0, "expected 0 errors, got {total_errors}");
    assert_eq!(report.count, 500, "expected 500 fetch_inbox calls");
    assert!(
        report.p95 < 500_000,
        "SLO: p95 < 500ms, got {:.1}ms",
        report.p95 as f64 / 1000.0
    );
}

// ---------------------------------------------------------------------------
// Scenario E: Inbox-stats polling cache effectiveness
// ---------------------------------------------------------------------------
// Compare two polling patterns for get_inbox_stats:
//   1) forced-miss polling (invalidate before each poll)
//   2) warm-cache polling (single cold miss, then repeated hits)
//
// Emits structured JSON so CI artifacts can be consumed by tooling.

#[test]
#[ignore = "benchmark scenario: inbox-stats polling cache effectiveness"]
fn load_scenario_e_inbox_stats_polling_cache_effectiveness() {
    let (pool, _dir) = make_load_pool(32);
    let polls: usize = 1000;
    let polls_u64 = u64::try_from(polls).expect("poll count fits u64");

    let project_id = block_on_with_retry(5, |cx| {
        let pp = pool.clone();
        let key = format!("/data/load/inbox_stats_polling_{}", unique_suffix());
        async move { queries::ensure_project(&cx, &pp, &key).await }
    })
    .id
    .unwrap();

    let sender_id = block_on_with_retry(5, |cx| {
        let pp = pool.clone();
        async move {
            queries::register_agent(
                &cx,
                &pp,
                project_id,
                "BoldCastle",
                "load-bench",
                "model",
                None,
                None,
                None,
            )
            .await
        }
    })
    .id
    .unwrap();

    let receiver_id = block_on_with_retry(5, |cx| {
        let pp = pool.clone();
        async move {
            queries::register_agent(
                &cx,
                &pp,
                project_id,
                "QuietLake",
                "load-bench",
                "model",
                None,
                None,
                None,
            )
            .await
        }
    })
    .id
    .unwrap();

    // Seed inbox_stats materialized row with a realistic payload.
    for i in 0..50 {
        let required_ack = i % 2 == 0;
        let out = block_on(|cx| {
            let pp = pool.clone();
            async move {
                queries::create_message_with_recipients(
                    &cx,
                    &pp,
                    project_id,
                    sender_id,
                    &format!("polling-seed-{i}"),
                    "seed body for inbox stats polling benchmark",
                    None,
                    "normal",
                    required_ack,
                    "",
                    &[(receiver_id, "to")],
                )
                .await
            }
        });
        assert!(
            matches!(out, Outcome::Ok(_)),
            "seed message creation failed at index {i}"
        );
    }

    QUERY_TRACKER.enable(None);
    QUERY_TRACKER.reset();

    read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), receiver_id);
    let forced_start = Instant::now();
    let (forced_report, forced_db_queries) =
        run_inbox_stats_polling_phase(&pool, receiver_id, polls, true);
    let forced_elapsed = forced_start.elapsed();

    QUERY_TRACKER.reset();
    read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), receiver_id);
    let warm_start = Instant::now();
    let (warm_report, warm_db_queries) =
        run_inbox_stats_polling_phase(&pool, receiver_id, polls, false);
    let warm_elapsed = warm_start.elapsed();

    QUERY_TRACKER.disable();
    QUERY_TRACKER.reset();
    read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), receiver_id);

    let forced_hit_ratio = (polls_u64.saturating_sub(forced_db_queries)) as f64 / polls_u64 as f64;
    let warm_hit_ratio = (polls_u64.saturating_sub(warm_db_queries)) as f64 / polls_u64 as f64;
    let query_reduction_factor = if warm_db_queries == 0 {
        forced_db_queries as f64
    } else {
        forced_db_queries as f64 / warm_db_queries as f64
    };

    eprintln!("\n=== Scenario E: Inbox Stats Polling Cache Effectiveness ===");
    forced_report.print("forced-miss polling");
    warm_report.print("warm-cache polling");
    eprintln!(
        "  forced elapsed={:.2}ms, warm elapsed={:.2}ms",
        forced_elapsed.as_secs_f64() * 1000.0,
        warm_elapsed.as_secs_f64() * 1000.0
    );
    eprintln!(
        "  DB queries (inbox_stats): forced={forced_db_queries}, warm={warm_db_queries}, reduction={query_reduction_factor:.2}x"
    );
    eprintln!(
        "  estimated hit ratio: forced={:.2}%, warm={:.2}%",
        forced_hit_ratio * 100.0,
        warm_hit_ratio * 100.0
    );

    let metrics = serde_json::json!({
        "scenario": "load_scenario_e_inbox_stats_polling_cache_effectiveness",
        "polls": polls,
        "forced_miss": {
            "count": forced_report.count,
            "p50_ms": forced_report.p50 as f64 / 1000.0,
            "p95_ms": forced_report.p95 as f64 / 1000.0,
            "p99_ms": forced_report.p99 as f64 / 1000.0,
            "max_ms": forced_report.max as f64 / 1000.0,
            "elapsed_ms": forced_elapsed.as_secs_f64() * 1000.0,
            "db_queries_inbox_stats": forced_db_queries,
            "estimated_cache_hit_ratio": forced_hit_ratio
        },
        "warm_cache": {
            "count": warm_report.count,
            "p50_ms": warm_report.p50 as f64 / 1000.0,
            "p95_ms": warm_report.p95 as f64 / 1000.0,
            "p99_ms": warm_report.p99 as f64 / 1000.0,
            "max_ms": warm_report.max as f64 / 1000.0,
            "elapsed_ms": warm_elapsed.as_secs_f64() * 1000.0,
            "db_queries_inbox_stats": warm_db_queries,
            "estimated_cache_hit_ratio": warm_hit_ratio
        },
        "comparison": {
            "query_reduction_factor": query_reduction_factor,
            "warm_vs_forced_p50_ratio": if forced_report.p50 == 0 {
                0.0
            } else {
                warm_report.p50 as f64 / forced_report.p50 as f64
            }
        }
    });
    eprintln!("BENCH_JSON {metrics}");

    assert!(
        forced_db_queries >= polls_u64.saturating_mul(95) / 100,
        "forced-miss polling should issue DB queries on almost every poll (got {forced_db_queries}/{polls})"
    );
    assert!(
        warm_db_queries <= polls_u64 / 20 + 2,
        "warm-cache polling should issue very few DB queries (got {warm_db_queries}/{polls})"
    );
    assert!(
        warm_hit_ratio > forced_hit_ratio,
        "warm-cache polling should yield a higher hit ratio (forced={forced_hit_ratio:.4}, warm={warm_hit_ratio:.4})"
    );
}

// ---------------------------------------------------------------------------
// Scenario F: Read-cache profile hotset retention
// ---------------------------------------------------------------------------
// Compare conservative and high-memory read-cache profile capacities on the
// same deterministic hotset. Misses simulate a DB fallback by reinserting the
// expected row, so both profiles must produce the same logical output stream
// while the high-memory profile should avoid most fallback work.

fn make_cache_profile_agent(idx: usize) -> AgentRow {
    let id = i64::try_from(idx + 1).expect("agent id fits i64");
    AgentRow {
        id: Some(id),
        project_id: 1,
        name: format!("HotAgent{idx:05}"),
        program: "load-bench".to_string(),
        model: "cache-profile".to_string(),
        task_description: "read-cache hotset profile benchmark".to_string(),
        inception_ts: 1_700_000_000_000_000,
        last_active_ts: 1_700_000_000_000_000,
        attachments_policy: "auto".to_string(),
        contact_policy: "auto".to_string(),
        reaper_exempt: 0,
        registration_token: None,
    }
}

fn run_cache_profile_hotset(
    profile: &'static str,
    capacity_per_category: usize,
    agents: &[AgentRow],
    passes: usize,
) -> CacheProfileHotsetReport {
    let cache = ReadCache::new_for_testing_with_capacity(capacity_per_category);
    for agent in agents {
        cache.put_agent(agent);
    }

    let probes = agents
        .len()
        .checked_mul(passes)
        .expect("probe count should fit usize");
    let mut hits = 0_u64;
    let mut misses = 0_u64;
    let mut checksum = 0_u64;
    let mut latencies = Vec::with_capacity(probes);

    for pass in 0..passes {
        for step in 0..agents.len() {
            let idx = (step.wrapping_mul(37).wrapping_add(pass.wrapping_mul(101))) % agents.len();
            let expected = &agents[idx];
            let t0 = Instant::now();
            let cached = cache.get_agent(expected.project_id, &expected.name);
            latencies.push(t0.elapsed().as_micros() as u64);

            if let Some(agent) = cached {
                hits += 1;
                assert_eq!(agent.id, expected.id, "cached agent id mismatch");
                checksum = checksum
                    .wrapping_mul(1_000_003)
                    .wrapping_add(agent.id.expect("agent id must exist") as u64);
            } else {
                misses += 1;
                cache.put_agent(expected);
                checksum = checksum
                    .wrapping_mul(1_000_003)
                    .wrapping_add(expected.id.expect("agent id must exist") as u64);
            }
        }
    }

    let latency = LatencyReport::from_latencies(&mut latencies, 0);
    let footprint = cache.footprint_estimate();
    CacheProfileHotsetReport {
        profile,
        capacity_per_category,
        seeded_agents: agents.len(),
        probes,
        hits,
        misses,
        hit_ratio: hits as f64 / probes as f64,
        lookup_p50_us: latency.p50,
        lookup_p95_us: latency.p95,
        lookup_p99_us: latency.p99,
        lookup_max_us: latency.max,
        output_checksum: checksum,
        final_live_entries: footprint.counts.total_live_entries(),
        capacity_utilization_bp: footprint.capacity_utilization_bp,
        total_estimated_bytes: footprint.total_estimated_bytes,
    }
}

#[test]
#[ignore = "benchmark scenario: read-cache profile hotset retention"]
fn load_scenario_f_read_cache_profile_hotset_retention() {
    let conservative_capacity = CacheProfile::Conservative.read_cache_entries_per_category();
    let high_memory_capacity = CacheProfile::HighMemory.read_cache_entries_per_category();
    let seeded_agents = 20_000;
    let passes = 3;
    assert!(
        conservative_capacity < seeded_agents,
        "conservative profile must be smaller than the synthetic hotset"
    );
    assert!(
        high_memory_capacity >= seeded_agents,
        "high-memory profile must fit the synthetic hotset"
    );

    let agents: Vec<_> = (0..seeded_agents).map(make_cache_profile_agent).collect();
    let conservative =
        run_cache_profile_hotset("conservative", conservative_capacity, &agents, passes);
    let high_memory =
        run_cache_profile_hotset("high-memory", high_memory_capacity, &agents, passes);

    eprintln!("\n=== Scenario F: Read Cache Profile Hotset Retention ===");
    eprintln!(
        "  conservative: hits={}, misses={}, hit_ratio={:.2}%, p95={}us, live_entries={}, util={}bp",
        conservative.hits,
        conservative.misses,
        conservative.hit_ratio * 100.0,
        conservative.lookup_p95_us,
        conservative.final_live_entries,
        conservative.capacity_utilization_bp
    );
    eprintln!(
        "  high-memory: hits={}, misses={}, hit_ratio={:.2}%, p95={}us, live_entries={}, util={}bp",
        high_memory.hits,
        high_memory.misses,
        high_memory.hit_ratio * 100.0,
        high_memory.lookup_p95_us,
        high_memory.final_live_entries,
        high_memory.capacity_utilization_bp
    );

    let miss_reduction_factor = if high_memory.misses == 0 {
        conservative.misses as f64
    } else {
        conservative.misses as f64 / high_memory.misses as f64
    };
    let report = serde_json::json!({
        "scenario": "load_scenario_f_read_cache_profile_hotset_retention",
        "bead": "br-n1wry",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "workload": {
            "seeded_agents": seeded_agents,
            "passes": passes,
            "probes": seeded_agents * passes,
            "lookup_order": "deterministic permutation: idx=(step*37 + pass*101) mod seeded_agents",
            "miss_policy": "simulate DB fallback by reinserting the expected row",
        },
        "profiles": {
            "conservative": conservative,
            "high_memory": high_memory,
        },
        "comparison": {
            "miss_reduction_factor": miss_reduction_factor,
        }
    });
    eprintln!("BENCH_JSON {report}");
    write_cache_profile_hotset_artifact(&report);

    let conservative = &report["profiles"]["conservative"];
    let high_memory = &report["profiles"]["high_memory"];
    assert_eq!(
        conservative["output_checksum"], high_memory["output_checksum"],
        "profile choice must not change logical lookup outputs"
    );
    assert!(
        high_memory["hit_ratio"].as_f64().expect("high hit ratio") >= 0.85,
        "high-memory profile should retain at least 85% of repeated hotset probes"
    );
    assert!(
        conservative["hit_ratio"]
            .as_f64()
            .expect("conservative hit ratio")
            < 0.10,
        "conservative profile should show measurable churn on this oversized hotset"
    );
    assert!(
        report["comparison"]["miss_reduction_factor"]
            .as_f64()
            .expect("miss reduction factor")
            >= 8.0,
        "high-memory profile should reduce simulated DB fallback misses by at least 8x"
    );
}
