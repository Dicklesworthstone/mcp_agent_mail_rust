//! Criterion benchmarks for the Git ref-integrity health sweep (br-66evl).
//!
//! The benchmark uses the same libgit2-backed fixture helpers as the
//! integration tests and calls the real sweep function. A small budget probe
//! writes a JSON artifact under `tests/artifacts/perf/` and fails the bench if
//! the healthy-repo budgets regress.

#![forbid(unsafe_code)]

use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use chrono::{TimeZone, Utc};
use criterion::{Criterion, criterion_group, criterion_main};
use mcp_agent_mail_server::tui_screens::system_health::{
    GitRefIntegrityProjectTarget, git_ref_integrity_sweep,
};
use mcp_agent_mail_test_helpers::repo::{self, RepoFixture};
use serde::Serialize;

const SINGLE_REPO_BUDGET_US: u64 = 200_000;
const SINGLE_REPO_P99_BUDGET_US: u64 = 500_000;
const LARGE_REPO_BUDGET_US: u64 = 1_500_000;
const FULL_ARCHIVE_PER_PROJECT_BUDGET_US: u64 = 200_000;
const BUDGET_SAMPLE_COUNT: usize = 15;

static ARTIFACT_WRITTEN: OnceLock<()> = OnceLock::new();

#[derive(Debug, Clone, Serialize)]
struct HealthSweepBenchSample {
    name: &'static str,
    projects: usize,
    commits_per_project: usize,
    samples_us: Vec<u64>,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    p95_per_project_us: u64,
    budget_us: u64,
    budget_passed: bool,
}

#[derive(Debug, Serialize)]
struct HealthSweepBenchArtifact {
    schema: &'static str,
    generated_at: String,
    command: &'static str,
    budgets: HealthSweepBenchBudgets,
    samples: Vec<HealthSweepBenchSample>,
}

#[derive(Debug, Serialize)]
struct HealthSweepBenchBudgets {
    unit: &'static str,
    single_repo_100_commits: HealthSweepBenchBudget,
    single_repo_1000_commits: HealthSweepBenchBudget,
    full_archive_50_projects_per_project: HealthSweepBenchBudget,
}

#[derive(Debug, Serialize)]
struct HealthSweepBenchBudget {
    p95: u64,
    p99: Option<u64>,
}

fn fixed_now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 11, 0, 0, 0)
        .single()
        .expect("valid fixed benchmark timestamp")
}

fn target(slug: impl Into<String>, repo: &RepoFixture) -> GitRefIntegrityProjectTarget {
    GitRefIntegrityProjectTarget::new(slug.into(), repo.path())
}

fn artifact_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/artifacts/perf")
}

fn artifact_path() -> PathBuf {
    let date = Utc::now().format("%Y-%m-%d");
    artifact_root().join(format!("health_sweep_bench_{date}.json"))
}

fn duration_us(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn percentile(sorted_samples: &[u64], percentile: usize) -> u64 {
    let index = sorted_samples
        .len()
        .saturating_sub(1)
        .saturating_mul(percentile)
        .div_ceil(100);
    sorted_samples.get(index).copied().unwrap_or(0)
}

fn timed_sweep_us(targets: &[GitRefIntegrityProjectTarget]) -> u64 {
    let started_at = Instant::now();
    let sweep = git_ref_integrity_sweep(
        targets,
        0,
        targets.len().max(1),
        true,
        900,
        false,
        &[],
        fixed_now(),
    );
    black_box(sweep.total_findings());
    duration_us(started_at)
}

fn collect_sample(
    name: &'static str,
    targets: &[GitRefIntegrityProjectTarget],
    commits_per_project: usize,
    sample_count: usize,
    budget_us: u64,
) -> HealthSweepBenchSample {
    let mut samples_us = (0..sample_count)
        .map(|_| timed_sweep_us(targets))
        .collect::<Vec<_>>();
    samples_us.sort_unstable();

    let projects = targets.len().max(1);
    let projects_u64 = u64::try_from(projects).unwrap_or(u64::MAX);
    let p95_us = percentile(&samples_us, 95);
    HealthSweepBenchSample {
        name,
        projects: targets.len(),
        commits_per_project,
        p50_us: percentile(&samples_us, 50),
        p95_us,
        p99_us: percentile(&samples_us, 99),
        max_us: samples_us.last().copied().unwrap_or(0),
        p95_per_project_us: p95_us / projects_u64.max(1),
        budget_us,
        budget_passed: p95_us <= budget_us,
        samples_us,
    }
}

fn write_budget_artifact_once() {
    ARTIFACT_WRITTEN.get_or_init(|| {
        let repo_100 = repo::with_commits(100);
        let repo_1000 = repo::with_commits(1_000);
        let archive_repos = (0..50).map(|_| repo::single_commit()).collect::<Vec<_>>();

        let targets_100 = vec![target("healthy-100", &repo_100)];
        let targets_1000 = vec![target("healthy-1000", &repo_1000)];
        let archive_targets = archive_repos
            .iter()
            .enumerate()
            .map(|(idx, fixture)| target(format!("healthy-{idx:02}"), fixture))
            .collect::<Vec<_>>();

        let single_100 = collect_sample(
            "single_project_100_commits",
            &targets_100,
            100,
            BUDGET_SAMPLE_COUNT,
            SINGLE_REPO_BUDGET_US,
        );
        let single_1000 = collect_sample(
            "single_project_1000_commits",
            &targets_1000,
            1_000,
            BUDGET_SAMPLE_COUNT,
            LARGE_REPO_BUDGET_US,
        );
        let full_archive = collect_sample(
            "full_archive_50_projects",
            &archive_targets,
            1,
            BUDGET_SAMPLE_COUNT,
            FULL_ARCHIVE_PER_PROJECT_BUDGET_US * 50,
        );

        assert!(
            single_100.p95_us <= SINGLE_REPO_BUDGET_US,
            "{} p95={}us exceeds budget={}us",
            single_100.name,
            single_100.p95_us,
            SINGLE_REPO_BUDGET_US
        );
        assert!(
            single_100.p99_us <= SINGLE_REPO_P99_BUDGET_US,
            "{} p99={}us exceeds budget={}us",
            single_100.name,
            single_100.p99_us,
            SINGLE_REPO_P99_BUDGET_US
        );
        assert!(
            single_1000.p95_us <= LARGE_REPO_BUDGET_US,
            "{} p95={}us exceeds budget={}us",
            single_1000.name,
            single_1000.p95_us,
            LARGE_REPO_BUDGET_US
        );
        assert!(
            full_archive.p95_per_project_us <= FULL_ARCHIVE_PER_PROJECT_BUDGET_US,
            "{} p95_per_project={}us exceeds budget={}us",
            full_archive.name,
            full_archive.p95_per_project_us,
            FULL_ARCHIVE_PER_PROJECT_BUDGET_US
        );

        let artifact = HealthSweepBenchArtifact {
            schema: "health_sweep_bench.v1",
            generated_at: Utc::now().to_rfc3339(),
            command: "rch exec -- cargo bench -p mcp-agent-mail-server --bench health_sweep_bench",
            budgets: HealthSweepBenchBudgets {
                unit: "microseconds",
                single_repo_100_commits: HealthSweepBenchBudget {
                    p95: SINGLE_REPO_BUDGET_US,
                    p99: Some(SINGLE_REPO_P99_BUDGET_US),
                },
                single_repo_1000_commits: HealthSweepBenchBudget {
                    p95: LARGE_REPO_BUDGET_US,
                    p99: None,
                },
                full_archive_50_projects_per_project: HealthSweepBenchBudget {
                    p95: FULL_ARCHIVE_PER_PROJECT_BUDGET_US,
                    p99: None,
                },
            },
            samples: vec![single_100, single_1000, full_archive],
        };

        let path = artifact_path();
        let parent = path.parent().expect("artifact path has parent");
        fs::create_dir_all(parent).expect("create perf artifact directory");
        let file = fs::File::create(&path).expect("create health sweep bench artifact");
        serde_json::to_writer_pretty(file, &artifact).expect("write health sweep bench artifact");
    });
}

fn bench_health_sweep_budget_artifact(c: &mut Criterion) {
    write_budget_artifact_once();
    c.bench_function("health_sweep_budget_artifact_probe", |b| {
        b.iter(write_budget_artifact_once);
    });
}

fn bench_health_sweep_single_100_commit_repo(c: &mut Criterion) {
    let repo = repo::with_commits(100);
    let targets = vec![target("healthy-100", &repo)];
    c.bench_function("health_sweep_single_100_commit_repo", |b| {
        b.iter(|| black_box(timed_sweep_us(&targets)));
    });
}

fn bench_health_sweep_single_1000_commit_repo(c: &mut Criterion) {
    let repo = repo::with_commits(1_000);
    let targets = vec![target("healthy-1000", &repo)];
    c.bench_function("health_sweep_single_1000_commit_repo", |b| {
        b.iter(|| black_box(timed_sweep_us(&targets)));
    });
}

fn bench_health_sweep_full_archive_50_projects(c: &mut Criterion) {
    let repos = (0..50).map(|_| repo::single_commit()).collect::<Vec<_>>();
    let targets = repos
        .iter()
        .enumerate()
        .map(|(idx, fixture)| target(format!("healthy-{idx:02}"), fixture))
        .collect::<Vec<_>>();
    c.bench_function("health_sweep_full_archive_50_projects", |b| {
        b.iter(|| black_box(timed_sweep_us(&targets)));
    });
}

criterion_group!(
    benches,
    bench_health_sweep_budget_artifact,
    bench_health_sweep_single_100_commit_repo,
    bench_health_sweep_single_1000_commit_repo,
    bench_health_sweep_full_archive_50_projects,
);
criterion_main!(benches);
