//! Criterion benchmarks for archive boot-time integrity checks.
//!
//! The budget probe writes a JSON artifact under `tests/artifacts/perf/` and
//! fails if healthy archive sweeps or gated safe-ref auto-repair regress beyond
//! the F8 boot-check budgets.

#![forbid(unsafe_code)]

use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::Utc;
use criterion::{Criterion, criterion_group, criterion_main};
use git2::{Repository, Signature};
use mcp_agent_mail_storage::boot_check::{BootCheckMode, preflight_archive_integrity};
use serde::Serialize;
use tempfile::TempDir;

const BUDGET_SAMPLE_COUNT: usize = 9;
const SINGLE_PROJECT_100_COMMITS_P95_US: u64 = 100_000;
const FULL_ARCHIVE_50_PROJECTS_P95_US: u64 = 5_000_000;
const AUTO_REPAIR_ORPHAN_STASH_P95_US: u64 = 500_000;

static ARTIFACT_WRITTEN: OnceLock<()> = OnceLock::new();

#[derive(Debug, Serialize)]
struct BootCheckBenchArtifact {
    schema: &'static str,
    generated_at: String,
    command: &'static str,
    budgets: BootCheckBenchBudgets,
    samples: Vec<BootCheckBenchSample>,
}

#[derive(Debug, Serialize)]
struct BootCheckBenchBudgets {
    unit: &'static str,
    single_project_100_commits_p95_us: u64,
    full_archive_50_projects_p95_us: u64,
    auto_repair_orphan_stash_p95_us: u64,
}

#[derive(Debug, Serialize)]
struct BootCheckBenchSample {
    name: &'static str,
    mode: &'static str,
    projects: usize,
    commits_per_project: usize,
    samples_us: Vec<u64>,
    p50_us: u64,
    p95_us: u64,
    max_us: u64,
    budget_us: u64,
    budget_passed: bool,
    findings_count: usize,
    auto_repaired_count: u32,
}

struct ArchiveFixture {
    root: TempDir,
}

impl ArchiveFixture {
    fn path(&self) -> &Path {
        self.root.path()
    }
}

fn artifact_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/artifacts/perf")
}

fn artifact_path() -> PathBuf {
    let date = Utc::now().format("%Y-%m-%d");
    artifact_root().join(format!("boot_check_bench_{date}.json"))
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

const fn mode_label(mode: BootCheckMode) -> &'static str {
    match mode {
        BootCheckMode::Warn => "warn",
        BootCheckMode::Abort => "abort",
        BootCheckMode::AutoRepair => "auto_repair",
    }
}

fn signature() -> Signature<'static> {
    Signature::now("boot-check-bench", "boot-check-bench@example.invalid").expect("signature")
}

fn init_repo_with_commits(path: &Path, commits: usize) {
    fs::create_dir_all(path).expect("create project repo dir");
    let repo = Repository::init(path).expect("init git repo");
    let mut cfg = repo.config().expect("repo config");
    cfg.set_str("user.name", "boot-check-bench")
        .expect("set user.name");
    cfg.set_str("user.email", "boot-check-bench@example.invalid")
        .expect("set user.email");

    let sig = signature();
    let mut parent = None;
    for idx in 0..commits.max(1) {
        let name = format!("file_{idx:04}.txt");
        fs::write(path.join(&name), format!("commit {idx}\n")).expect("write fixture file");
        let mut index = repo.index().expect("repo index");
        index.add_path(Path::new(&name)).expect("add fixture file");
        index.write().expect("write index");
        let tree_oid = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_oid).expect("find tree");
        let parents = parent
            .map(|oid| vec![repo.find_commit(oid).expect("find parent commit")])
            .unwrap_or_default();
        let parent_refs = parents.iter().collect::<Vec<_>>();
        let oid = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                &format!("bench commit {idx}"),
                &tree,
                &parent_refs,
            )
            .expect("commit fixture");
        parent = Some(oid);
    }
}

fn archive_with_projects(project_count: usize, commits_per_project: usize) -> ArchiveFixture {
    let root = TempDir::new().expect("archive tempdir");
    let projects = root.path().join("projects");
    fs::create_dir_all(&projects).expect("create projects dir");
    for idx in 0..project_count {
        init_repo_with_commits(
            &projects.join(format!("healthy_{idx:02}")),
            commits_per_project,
        );
    }
    ArchiveFixture { root }
}

fn archive_with_orphan_stash() -> ArchiveFixture {
    let fixture = archive_with_projects(1, 1);
    let stash_ref = fixture
        .path()
        .join("projects")
        .join("healthy_00")
        .join(".git")
        .join("refs")
        .join("stash");
    fs::write(stash_ref, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n")
        .expect("write orphan stash ref");
    fixture
}

fn timed_preflight_us(root: &Path, mode: BootCheckMode) -> (u64, usize, u32) {
    let started_at = Instant::now();
    let report = preflight_archive_integrity(root, mode);
    let elapsed_us = duration_us(started_at);
    let findings_count = report.findings.len();
    let auto_repaired_count = report.auto_repaired_count;
    black_box(&report);
    (elapsed_us, findings_count, auto_repaired_count)
}

fn collect_sample<F>(
    name: &'static str,
    mode: BootCheckMode,
    projects: usize,
    commits_per_project: usize,
    budget_us: u64,
    fixture_factory: F,
) -> BootCheckBenchSample
where
    F: Fn() -> ArchiveFixture,
{
    let mut samples = Vec::with_capacity(BUDGET_SAMPLE_COUNT);
    let mut findings_count = 0;
    let mut auto_repaired_count = 0;
    for _ in 0..BUDGET_SAMPLE_COUNT {
        let fixture = fixture_factory();
        let (elapsed_us, findings, auto_repaired) = timed_preflight_us(fixture.path(), mode);
        samples.push(elapsed_us);
        findings_count = findings;
        auto_repaired_count = auto_repaired;
    }
    samples.sort_unstable();
    let p95_us = percentile(&samples, 95);
    BootCheckBenchSample {
        name,
        mode: mode_label(mode),
        projects,
        commits_per_project,
        p50_us: percentile(&samples, 50),
        p95_us,
        max_us: samples.last().copied().unwrap_or(0),
        budget_us,
        budget_passed: p95_us <= budget_us,
        samples_us: samples,
        findings_count,
        auto_repaired_count,
    }
}

fn write_budget_artifact_once() {
    ARTIFACT_WRITTEN.get_or_init(|| {
        let single_project = collect_sample(
            "single_project_100_commits",
            BootCheckMode::Warn,
            1,
            100,
            SINGLE_PROJECT_100_COMMITS_P95_US,
            || archive_with_projects(1, 100),
        );
        let full_archive = collect_sample(
            "full_archive_50_projects",
            BootCheckMode::Warn,
            50,
            1,
            FULL_ARCHIVE_50_PROJECTS_P95_US,
            || archive_with_projects(50, 1),
        );
        let auto_repair = collect_sample(
            "auto_repair_orphan_stash",
            BootCheckMode::AutoRepair,
            1,
            1,
            AUTO_REPAIR_ORPHAN_STASH_P95_US,
            archive_with_orphan_stash,
        );

        assert!(
            single_project.p95_us <= SINGLE_PROJECT_100_COMMITS_P95_US,
            "{} p95={}us exceeds budget={}us",
            single_project.name,
            single_project.p95_us,
            SINGLE_PROJECT_100_COMMITS_P95_US
        );
        assert_eq!(
            single_project.findings_count, 0,
            "{} should stay healthy",
            single_project.name
        );
        assert!(
            full_archive.p95_us <= FULL_ARCHIVE_50_PROJECTS_P95_US,
            "{} p95={}us exceeds budget={}us",
            full_archive.name,
            full_archive.p95_us,
            FULL_ARCHIVE_50_PROJECTS_P95_US
        );
        assert_eq!(
            full_archive.findings_count, 0,
            "{} should stay healthy",
            full_archive.name
        );
        assert!(
            auto_repair.p95_us <= AUTO_REPAIR_ORPHAN_STASH_P95_US,
            "{} p95={}us exceeds budget={}us",
            auto_repair.name,
            auto_repair.p95_us,
            AUTO_REPAIR_ORPHAN_STASH_P95_US
        );
        assert_eq!(
            auto_repair.findings_count, 0,
            "{} should clear findings after repair",
            auto_repair.name
        );
        assert_eq!(
            auto_repair.auto_repaired_count, 1,
            "{} should repair one project",
            auto_repair.name
        );

        let artifact = BootCheckBenchArtifact {
            schema: "boot_check_bench.v1",
            generated_at: Utc::now().to_rfc3339(),
            command: "rch exec -- cargo bench -p mcp-agent-mail-storage --bench boot_check_bench",
            budgets: BootCheckBenchBudgets {
                unit: "microseconds",
                single_project_100_commits_p95_us: SINGLE_PROJECT_100_COMMITS_P95_US,
                full_archive_50_projects_p95_us: FULL_ARCHIVE_50_PROJECTS_P95_US,
                auto_repair_orphan_stash_p95_us: AUTO_REPAIR_ORPHAN_STASH_P95_US,
            },
            samples: vec![single_project, full_archive, auto_repair],
        };

        let path = artifact_path();
        let parent = path.parent().expect("artifact path has parent");
        fs::create_dir_all(parent).expect("create perf artifact directory");
        let file = fs::File::create(&path).expect("create boot check bench artifact");
        serde_json::to_writer_pretty(file, &artifact).expect("write boot check bench artifact");
    });
}

fn bench_boot_check_budget_artifact(c: &mut Criterion) {
    write_budget_artifact_once();
    c.bench_function("boot_check_budget_artifact_probe", |b| {
        b.iter(write_budget_artifact_once);
    });
}

fn bench_boot_check_single_project_100_commits(c: &mut Criterion) {
    let fixture = archive_with_projects(1, 100);
    c.bench_function("boot_check_single_project_100_commits", |b| {
        b.iter(|| black_box(timed_preflight_us(fixture.path(), BootCheckMode::Warn)));
    });
}

fn bench_boot_check_full_archive_50_projects(c: &mut Criterion) {
    let fixture = archive_with_projects(50, 1);
    c.bench_function("boot_check_full_archive_50_projects", |b| {
        b.iter(|| black_box(timed_preflight_us(fixture.path(), BootCheckMode::Warn)));
    });
}

fn bench_boot_check_auto_repair_orphan_stash(c: &mut Criterion) {
    c.bench_function("boot_check_auto_repair_orphan_stash", |b| {
        b.iter_custom(|iters| {
            let started_at = Instant::now();
            for _ in 0..iters {
                let fixture = archive_with_orphan_stash();
                black_box(timed_preflight_us(
                    fixture.path(),
                    BootCheckMode::AutoRepair,
                ));
            }
            started_at.elapsed()
        });
    });
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(300))
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets =
        bench_boot_check_budget_artifact,
        bench_boot_check_single_project_100_commits,
        bench_boot_check_full_archive_50_projects,
        bench_boot_check_auto_repair_orphan_stash,
}
criterion_main!(benches);
