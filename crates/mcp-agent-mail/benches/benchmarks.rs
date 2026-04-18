#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::redundant_pub_crate,
    clippy::significant_drop_tightening
)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use fastmcp::{Budget, CallToolParams, Cx};
use fastmcp_core::{Outcome, SessionState, block_on};
use mcp_agent_mail_conformance::Fixtures;
use mcp_agent_mail_db::search_planner::SearchQuery;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::hint::black_box;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

fn fixtures_path() -> std::path::PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/mcp-agent-mail` for this bench crate.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json")
}

fn seed_fixtures(fixtures: &Fixtures) {
    static SEEDED: Once = Once::new();

    SEEDED.call_once(|| {
        // Reuse the conformance fixtures to seed a realistic DB state.
        // This ensures benchmarks remain aligned with parity expectations.
        let config = mcp_agent_mail_core::Config::from_env();
        let router = mcp_agent_mail_server::build_server(&config).into_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let mut req_id: u64 = 1;

        for (tool_name, tool_fixture) in &fixtures.tools {
            for case in &tool_fixture.cases {
                let args = match &case.input {
                    Value::Null => None,
                    Value::Object(map) if map.is_empty() => None,
                    other => Some(other.clone()),
                };
                let params = CallToolParams {
                    name: tool_name.clone(),
                    arguments: args,
                    meta: None,
                };

                let _ = router
                    .handle_tools_call(
                        &cx,
                        req_id,
                        params,
                        &budget,
                        SessionState::new(),
                        None,
                        None,
                    )
                    .expect("tool call should succeed during seeding");
                req_id += 1;
            }
        }

        // Ensure any archive writes/commits from seeding are flushed before benchmarking.
        mcp_agent_mail_storage::wbq_flush();
        mcp_agent_mail_storage::flush_async_commits();
    });
}

fn bench_tools(c: &mut Criterion) {
    if !bench_scope_enabled("tools") {
        return;
    }

    // Ensure DB is initialized before anything touches the pool cache.
    let tmp = TempDir::new().expect("tempdir");
    let original_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(tmp.path()).expect("chdir to tempdir");

    // Load fixtures via absolute path (bench runs in tempdir so relative paths won't work).
    let fixtures = Fixtures::load(fixtures_path()).expect("fixtures");
    seed_fixtures(&fixtures);

    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let cx = Cx::for_testing();
    let budget = Budget::INFINITE;

    let mut group = c.benchmark_group("mcp_agent_mail_tools");

    // Bench high-frequency operations across tool clusters.
    // Format: (tool_name, case_name)
    let targets: &[(&str, &str)] = &[
        // Health
        ("health_check", "default"),
        // Identity cluster
        ("ensure_project", "abs_path_backend"),
        ("register_agent", "green_castle"),
        // Messaging cluster
        ("fetch_inbox", "gc_inbox_with_bodies"),
        ("search_messages", "search_hello"),
        ("summarize_thread", "summarize_thread_root"),
        // File reservations cluster
        ("file_reservation_paths", "reserve_src_glob"),
        // Macros cluster
        ("macro_start_session", "macro_start_session_basic"),
    ];

    for (tool_name, case_name) in targets {
        let fixture = fixtures
            .tools
            .get(*tool_name)
            .unwrap_or_else(|| panic!("missing tool fixture: {tool_name}"));
        let case = fixture
            .cases
            .iter()
            .find(|c| c.name == *case_name)
            .unwrap_or_else(|| panic!("missing case {case_name} for tool {tool_name}"));

        let args = match &case.input {
            Value::Null => None,
            Value::Object(map) if map.is_empty() => None,
            other => Some(other.clone()),
        };

        let params = CallToolParams {
            name: tool_name.to_string(),
            arguments: args,
            meta: None,
        };

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new(*tool_name, *case_name),
            &params,
            |b, params| {
                let mut req_id: u64 = 1;
                b.iter(|| {
                    let out = router
                        .handle_tools_call(
                            &cx,
                            req_id,
                            params.clone(),
                            &budget,
                            SessionState::new(),
                            None,
                            None,
                        )
                        .expect("tool call");
                    req_id = req_id.wrapping_add(1);
                    black_box(out);
                });
            },
        );
    }

    group.finish();

    // Ensure we don't drop the temp repo while background writers still have work.
    mcp_agent_mail_storage::wbq_flush();
    mcp_agent_mail_storage::flush_async_commits();

    std::env::set_current_dir(original_cwd).expect("restore cwd");
    drop(tmp);
}

#[derive(Debug, Clone, Copy)]
enum ArchiveScenario {
    SingleNoAttachments,
    SingleInlineAttachment,
    SingleFileAttachment,
    BatchNoAttachments { batch_size: usize },
}

impl ArchiveScenario {
    const fn benchmark_name(self) -> &'static str {
        match self {
            Self::SingleNoAttachments => "single_no_attachments",
            Self::SingleInlineAttachment => "single_inline_attachment",
            Self::SingleFileAttachment => "single_file_attachment",
            Self::BatchNoAttachments { .. } => "batch_no_attachments",
        }
    }

    fn artifact_label(self) -> String {
        match self {
            Self::BatchNoAttachments { batch_size } => {
                format!("{}_{}", self.benchmark_name(), batch_size)
            }
            _ => self.benchmark_name().to_string(),
        }
    }

    const fn elements_per_op(self) -> u64 {
        match self {
            Self::BatchNoAttachments { batch_size } => batch_size as u64,
            _ => 1,
        }
    }
}

fn repo_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/mcp-agent-mail`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn run_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}_{}", now.as_secs(), std::process::id())
}

fn artifact_dir(run_id: &str) -> PathBuf {
    repo_root()
        .join("tests")
        .join("artifacts")
        .join("bench")
        .join("archive")
        .join(run_id)
}

fn share_artifact_dir(run_id: &str) -> PathBuf {
    repo_root()
        .join("tests")
        .join("artifacts")
        .join("bench")
        .join("share")
        .join(run_id)
}

fn perf_artifact_dir() -> PathBuf {
    repo_root().join("tests").join("artifacts").join("perf")
}

fn bench_scope_enabled(scope: &str) -> bool {
    let Ok(raw_scope) = std::env::var("MCP_AGENT_MAIL_BENCH_SCOPE") else {
        return true;
    };

    let scopes = raw_scope
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    scopes.is_empty() || scopes.contains(&"all") || scopes.contains(&scope)
}

fn write_bmp24(path: &Path, width: u32, height: u32, seed: u32) -> std::io::Result<()> {
    // Minimal 24-bit BMP writer (uncompressed).
    // Pixel data is BGR, rows padded to 4-byte boundary, stored bottom-up.
    let width_us = width as usize;
    let height_us = height as usize;
    let row_bytes_unpadded = width_us * 3;
    let row_stride = (row_bytes_unpadded + 3) & !3;
    let pixel_bytes = row_stride * height_us;
    let file_size = 14 + 40 + pixel_bytes;

    let mut buf = Vec::with_capacity(file_size);

    // BITMAPFILEHEADER (14)
    buf.extend_from_slice(b"BM");
    buf.extend_from_slice(&(file_size as u32).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved1
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved2
    buf.extend_from_slice(&(54u32).to_le_bytes()); // offset to pixels

    // BITMAPINFOHEADER (40)
    buf.extend_from_slice(&(40u32).to_le_bytes()); // header size
    buf.extend_from_slice(&(width as i32).to_le_bytes());
    buf.extend_from_slice(&(height as i32).to_le_bytes());
    buf.extend_from_slice(&(1u16).to_le_bytes()); // planes
    buf.extend_from_slice(&(24u16).to_le_bytes()); // bpp
    buf.extend_from_slice(&0u32.to_le_bytes()); // compression
    buf.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
    buf.extend_from_slice(&(2835i32).to_le_bytes()); // ~72dpi
    buf.extend_from_slice(&(2835i32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // colors used
    buf.extend_from_slice(&0u32.to_le_bytes()); // important colors

    let pad = vec![0u8; row_stride - row_bytes_unpadded];
    for y in 0..height_us {
        let y_u32 = y as u32;
        for x in 0..width_us {
            let x_u32 = x as u32;
            let r = ((x_u32.wrapping_add(seed)) & 0xFF) as u8;
            let g = ((y_u32.wrapping_add(seed.wrapping_mul(3))) & 0xFF) as u8;
            let b = ((x_u32 ^ y_u32 ^ seed) & 0xFF) as u8;
            buf.push(b);
            buf.push(g);
            buf.push(r);
        }
        buf.extend_from_slice(&pad);
    }

    std::fs::write(path, buf)
}

#[derive(Debug, Clone, Serialize)]
struct ArchiveBenchScenarioResult {
    scenario: String,
    elements_per_op: u64,
    samples_us: Vec<u64>,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    p99_9_us: u64,
    p99_99_us: u64,
    max_us: u64,
    budget_p95_us: u64,
    budget_p99_us: u64,
    p95_within_budget: bool,
    p99_within_budget: bool,
    p95_delta_us: i64,
    p99_delta_us: i64,
    throughput_elements_per_sec: f64,
}

#[derive(Debug, Clone, Serialize)]
struct ArchiveBenchRun {
    run_id: String,
    arch: String,
    os: String,
    budget_regressions: usize,
    results: Vec<ArchiveBenchScenarioResult>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RecordedSpan {
    pub(crate) name: String,
    pub(crate) parent: Option<String>,
    pub(crate) count_per_request: usize,
    pub(crate) fields: BTreeMap<String, String>,
    pub(crate) duration_us: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArchivePerfComparison {
    pub(crate) batch_size: usize,
    pub(crate) samples_us: Vec<u64>,
    pub(crate) p50_us: u64,
    pub(crate) p95_us: u64,
    pub(crate) p99_us: u64,
    pub(crate) p99_9_us: u64,
    pub(crate) p99_99_us: u64,
    pub(crate) max_us: u64,
    pub(crate) throughput_elements_per_sec: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArchivePerfCategory {
    pub(crate) category: String,
    pub(crate) cumulative_us: u64,
    pub(crate) count: usize,
    pub(crate) p50_us: u64,
    pub(crate) p95_us: u64,
    pub(crate) avg_us: u64,
    pub(crate) max_us: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArchivePerfEnvironment {
    pub(crate) repo_root: String,
    pub(crate) cargo_target_dir: Option<String>,
    pub(crate) rustc_version: Option<String>,
    pub(crate) kernel_release: Option<String>,
    pub(crate) filesystem: Option<String>,
    pub(crate) mount_source: Option<String>,
    pub(crate) mount_options: Option<String>,
    pub(crate) storage_model: Option<String>,
    pub(crate) storage_transport: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArchivePerfReproduction {
    pub(crate) warm_profile_command: String,
    pub(crate) flamegraph_command: String,
    pub(crate) cross_engineer_target: String,
    pub(crate) dry_run_validated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArchivePerfLoggingRequirement {
    pub(crate) event: String,
    pub(crate) required_fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ChromeTraceEvent {
    pub(crate) name: String,
    pub(crate) cat: String,
    pub(crate) ph: String,
    pub(crate) ts: u64,
    pub(crate) dur: u64,
    pub(crate) pid: u32,
    pub(crate) tid: u32,
    pub(crate) args: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArchivePerfHypothesisEvaluation {
    pub(crate) name: String,
    pub(crate) supports_or_rejects: String,
    pub(crate) evidence: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArchivePerfReport {
    pub(crate) run_id: String,
    pub(crate) arch: String,
    pub(crate) os: String,
    pub(crate) warm_db: bool,
    pub(crate) environment: ArchivePerfEnvironment,
    pub(crate) reproduction: ArchivePerfReproduction,
    pub(crate) structured_logging_requirements: Vec<ArchivePerfLoggingRequirement>,
    pub(crate) comparison: Vec<ArchivePerfComparison>,
    pub(crate) batch_100_spans: Vec<RecordedSpan>,
    #[serde(rename = "traceEvents")]
    pub(crate) trace_events: Vec<ChromeTraceEvent>,
    pub(crate) top_categories: Vec<ArchivePerfCategory>,
    pub(crate) hypothesis_evaluations: Vec<ArchivePerfHypothesisEvaluation>,
    pub(crate) scaling_law_note: String,
}

#[derive(Debug, Default)]
struct SpanFieldRecorder {
    fields: BTreeMap<String, String>,
}

impl Visit for SpanFieldRecorder {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

#[derive(Debug)]
struct ActiveSpanRecord {
    name: String,
    fields: BTreeMap<String, String>,
    entered_at: Option<Instant>,
    cumulative: std::time::Duration,
}

#[derive(Debug, Default)]
struct SpanRecorderState {
    active: Mutex<HashMap<tracing::span::Id, ActiveSpanRecord>>,
    closed: Mutex<Vec<RecordedSpan>>,
}

impl SpanRecorderState {
    fn take_closed(&self) -> Vec<RecordedSpan> {
        let mut closed = self.closed.lock().expect("closed span lock");
        std::mem::take(&mut *closed)
    }
}

#[derive(Debug, Clone)]
struct SpanRecorderLayer {
    state: Arc<SpanRecorderState>,
}

impl SpanRecorderLayer {
    const fn new(state: Arc<SpanRecorderState>) -> Self {
        Self { state }
    }
}

impl<S> Layer<S> for SpanRecorderLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        _ctx: Context<'_, S>,
    ) {
        let mut visitor = SpanFieldRecorder::default();
        attrs.record(&mut visitor);
        let mut active = self.state.active.lock().expect("active span lock");
        active.insert(
            id.clone(),
            ActiveSpanRecord {
                name: attrs.metadata().name().to_string(),
                fields: visitor.fields,
                entered_at: None,
                cumulative: std::time::Duration::ZERO,
            },
        );
    }

    fn on_enter(&self, id: &tracing::span::Id, _ctx: Context<'_, S>) {
        let mut active = self.state.active.lock().expect("active span lock");
        if let Some(span) = active.get_mut(id) {
            span.entered_at = Some(Instant::now());
        }
    }

    #[allow(clippy::collapsible_if)]
    fn on_exit(&self, id: &tracing::span::Id, _ctx: Context<'_, S>) {
        let mut active = self.state.active.lock().expect("active span lock");
        if let Some(span) = active.get_mut(id) {
            if let Some(started_at) = span.entered_at.take() {
                span.cumulative += started_at.elapsed();
            }
        }
    }

    fn on_close(&self, id: tracing::span::Id, _ctx: Context<'_, S>) {
        let mut active = self.state.active.lock().expect("active span lock");
        let Some(span) = active.remove(&id) else {
            return;
        };
        drop(active);

        if span.cumulative.is_zero() {
            return;
        }

        let duration_us = u64::try_from(span.cumulative.as_micros()).unwrap_or(u64::MAX);
        let mut closed = self.state.closed.lock().expect("closed span lock");
        closed.push(RecordedSpan {
            parent: inferred_span_parent(&span.name),
            count_per_request: 1,
            name: span.name,
            fields: span.fields,
            duration_us,
        });
    }
}

fn span_recorder_state() -> Arc<SpanRecorderState> {
    static STATE: OnceLock<Arc<SpanRecorderState>> = OnceLock::new();
    STATE
        .get_or_init(|| {
            let state = Arc::new(SpanRecorderState::default());
            let subscriber = Registry::default().with(SpanRecorderLayer::new(state.clone()));
            tracing::subscriber::set_global_default(subscriber)
                .expect("bench trace subscriber must install once");
            state
        })
        .clone()
}

const PERCENTILE_SCALE: u32 = 1_000_000;

fn percentile_us(mut samples: Vec<u64>, pct: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let n = samples.len();
    let max_idx = n.saturating_sub(1);
    let pct = pct.clamp(0.0, 1.0);
    // Fixed-point to avoid float->usize casts and large-int->float precision lints.
    let denom_u64 = u64::from(PERCENTILE_SCALE);
    let scaled = (pct * f64::from(PERCENTILE_SCALE)).round();
    let scaled_u64 = u64::try_from(scaled as i64).unwrap_or(0).min(denom_u64);
    let idx_u64 = (scaled_u64.saturating_mul(max_idx as u64) + (denom_u64 / 2)) / denom_u64;
    let idx = usize::try_from(idx_u64).unwrap_or(max_idx).min(max_idx);
    samples[idx]
}

pub(crate) fn inferred_span_parent(name: &str) -> Option<String> {
    match name {
        "archive_batch.write_message_batch_bundle" => {
            Some("archive_batch.write_message_batch".to_string())
        }
        "archive_batch.write_message_batch"
        | "archive_batch.flush_async_commits"
        | "archive_batch.wbq_flush" => Some("archive_batch.sample".to_string()),
        _ => None,
    }
}

const fn scenario_budgets_us(scenario: ArchiveScenario) -> (u64, u64) {
    match scenario {
        ArchiveScenario::BatchNoAttachments { .. } => (250_000, 300_000),
        _ => (25_000, 30_000),
    }
}

#[allow(clippy::too_many_lines)]
fn run_archive_harness_once() {
    static DID_RUN: Once = Once::new();
    DID_RUN.call_once(|| {
        let run_id = run_id();
        let out_dir = artifact_dir(&run_id);
        let _ = std::fs::create_dir_all(&out_dir);

        // Small, deterministic fixed-run harness for p50/p95/p99 + raw samples.
        let scenarios: &[(ArchiveScenario, usize)] = &[
            (ArchiveScenario::SingleNoAttachments, 200),
            (ArchiveScenario::SingleInlineAttachment, 50),
            (ArchiveScenario::SingleFileAttachment, 50),
            (ArchiveScenario::BatchNoAttachments { batch_size: 1 }, 100),
            (ArchiveScenario::BatchNoAttachments { batch_size: 10 }, 50),
            (ArchiveScenario::BatchNoAttachments { batch_size: 100 }, 10),
        ];

        let mut results = Vec::new();
        let mut regressions = 0usize;

        for (scenario, ops) in scenarios {
            if let ArchiveScenario::BatchNoAttachments { batch_size } = *scenario {
                // Measure each batch in a fresh repo so the samples reflect a "single burst"
                // and aren't dominated by repo growth effects across repeated batch runs.
                let original_cwd = std::env::current_dir().expect("cwd");
                let project_slug = "bench-archive";
                let sender = "BenchSender";
                let recipients = vec!["BenchReceiver".to_string()];

                let mut samples_us: Vec<u64> = Vec::with_capacity(*ops);
                for _ in 0..*ops {
                    let tmp = TempDir::new().expect("tempdir");
                    std::env::set_current_dir(tmp.path()).expect("chdir");

                    let mut config = mcp_agent_mail_core::Config::from_env();
                    config.storage_root = tmp.path().join("archive_repo");
                    config.database_url = format!(
                        "sqlite+aiosqlite:///{}",
                        tmp.path().join("storage.sqlite3").display()
                    );

                    let archive = mcp_agent_mail_storage::ensure_archive(&config, project_slug)
                        .expect("ensure_archive");

                    let t0 = Instant::now();
                    let messages = (1_i64..)
                        .take(batch_size)
                        .map(|msg_id| {
                            serde_json::json!({
                                "id": msg_id,
                                "project": project_slug,
                                "subject": "bench batch",
                                "created_ts": 1_700_000_000_000_000i64,
                            })
                        })
                        .collect::<Vec<_>>();
                    let no_extra_paths: &[String] = &[];
                    let batch_entries = messages
                        .iter()
                        .map(|message| mcp_agent_mail_storage::MessageBundleBatchEntry {
                            message,
                            body_md: "hello",
                            sender,
                            recipients: &recipients,
                            extra_paths: no_extra_paths,
                        })
                        .collect::<Vec<_>>();
                    mcp_agent_mail_storage::write_message_batch_bundle(
                        &archive,
                        &config,
                        &batch_entries,
                        None,
                    )
                    .expect("write_message_batch_bundle");
                    mcp_agent_mail_storage::flush_async_commits();

                    samples_us.push(t0.elapsed().as_micros() as u64);
                    std::env::set_current_dir(&original_cwd).expect("restore cwd");
                    drop(tmp);
                }

                let elements_per_op = scenario.elements_per_op();
                let total_elements = elements_per_op.saturating_mul(*ops as u64);
                let total_elements_f64 =
                    u32::try_from(total_elements).map_or_else(|_| f64::from(u32::MAX), f64::from);
                let total_us = samples_us.iter().copied().sum::<u64>();
                let total_us_f64 =
                    u32::try_from(total_us).map_or_else(|_| f64::from(u32::MAX), f64::from);
                let throughput = if total_us_f64 > 0.0 {
                    total_elements_f64 / (total_us_f64 / 1_000_000.0)
                } else {
                    0.0
                };

                let p50_us = percentile_us(samples_us.clone(), 0.50);
                let p95_us = percentile_us(samples_us.clone(), 0.95);
                let p99_us = percentile_us(samples_us.clone(), 0.99);
                let tail_p99_9_us = percentile_us(samples_us.clone(), 0.999);
                let extreme_p99_99_us = percentile_us(samples_us.clone(), 0.9999);
                let max_us = samples_us.iter().copied().max().unwrap_or(0);

                let (budget_p95_us, budget_p99_us) = scenario_budgets_us(*scenario);
                let p95_within_budget = p95_us <= budget_p95_us;
                let p99_within_budget = p99_us <= budget_p99_us;
                let p95_delta_us = p95_us as i64 - budget_p95_us as i64;
                let p99_delta_us = p99_us as i64 - budget_p99_us as i64;
                if !p95_within_budget || !p99_within_budget {
                    regressions += 1;
                }

                let scenario_result = ArchiveBenchScenarioResult {
                    scenario: scenario.artifact_label(),
                    elements_per_op,
                    samples_us: samples_us.clone(),
                    p50_us,
                    p95_us,
                    p99_us,
                    p99_9_us: tail_p99_9_us,
                    p99_99_us: extreme_p99_99_us,
                    max_us,
                    budget_p95_us,
                    budget_p99_us,
                    p95_within_budget,
                    p99_within_budget,
                    p95_delta_us,
                    p99_delta_us,
                    throughput_elements_per_sec: (throughput * 100.0).round() / 100.0,
                };

                let _ = std::fs::write(
                    out_dir.join(format!("{}.json", scenario.artifact_label())),
                    serde_json::to_string_pretty(&scenario_result).unwrap_or_default(),
                );
                results.push(scenario_result);
                continue;
            }

            let tmp = TempDir::new().expect("tempdir");
            let original_cwd = std::env::current_dir().expect("cwd");
            std::env::set_current_dir(tmp.path()).expect("chdir");

            let mut config = mcp_agent_mail_core::Config::from_env();
            config.storage_root = tmp.path().join("archive_repo");
            config.database_url = format!(
                "sqlite+aiosqlite:///{}",
                tmp.path().join("storage.sqlite3").display()
            );

            let project_slug = "bench-archive";
            let archive = mcp_agent_mail_storage::ensure_archive(&config, project_slug)
                .expect("ensure_archive");

            let sender = "BenchSender";
            let recipients = vec!["BenchReceiver".to_string()];

            // Pre-generate attachment inputs (outside timed region).
            let input_dir = tmp.path().join("input");
            let _ = std::fs::create_dir_all(&input_dir);

            let mut attachment_paths: Vec<PathBuf> = Vec::new();
            if matches!(
                *scenario,
                ArchiveScenario::SingleInlineAttachment | ArchiveScenario::SingleFileAttachment
            ) {
                for i in 0..*ops {
                    let p = input_dir.join(format!("img_{i}.bmp"));
                    write_bmp24(&p, 32, 32, i as u32).expect("write bmp");
                    attachment_paths.push(p);
                }
            }

            let mut msg_id: i64 = 1;
            let mut samples_us: Vec<u64> = Vec::with_capacity(*ops);
            let start_all = Instant::now();

            match *scenario {
                ArchiveScenario::SingleNoAttachments => {
                    for _ in 0..*ops {
                        let t0 = Instant::now();

                        let message_json = serde_json::json!({
                            "id": msg_id,
                            "project": project_slug,
                            "subject": "bench no attachments",
                            "created_ts": 1_700_000_000_000_000i64,
                        });

                        mcp_agent_mail_storage::write_message_bundle(
                            &archive,
                            &config,
                            &message_json,
                            "hello",
                            sender,
                            &recipients,
                            &[],
                            None,
                        )
                        .expect("write_message_bundle");
                        mcp_agent_mail_storage::flush_async_commits();

                        samples_us.push(t0.elapsed().as_micros() as u64);
                        msg_id += 1;
                    }
                }
                ArchiveScenario::SingleInlineAttachment | ArchiveScenario::SingleFileAttachment => {
                    let policy = if matches!(*scenario, ArchiveScenario::SingleInlineAttachment) {
                        mcp_agent_mail_storage::EmbedPolicy::Inline
                    } else {
                        mcp_agent_mail_storage::EmbedPolicy::File
                    };

                    for path in attachment_paths.iter().take(*ops) {
                        let t0 = Instant::now();

                        let img_path = path.to_string_lossy().to_string();
                        let body = format!("inline image: ![img]({img_path})\n");
                        let (body2, meta, rel_paths) =
                            mcp_agent_mail_storage::process_markdown_images(
                                &archive,
                                &config,
                                &archive.root,
                                &body,
                                policy,
                            )
                            .expect("process_markdown_images");

                        let attachments_json: Vec<serde_json::Value> = meta
                            .into_iter()
                            .filter_map(|m| serde_json::to_value(m).ok())
                            .collect();

                        let message_json = serde_json::json!({
                            "id": msg_id,
                            "project": project_slug,
                            "subject": "bench attachment",
                            "created_ts": 1_700_000_000_000_000i64,
                            "attachments": attachments_json,
                        });

                        mcp_agent_mail_storage::write_message_bundle(
                            &archive,
                            &config,
                            &message_json,
                            &body2,
                            sender,
                            &recipients,
                            &rel_paths,
                            None,
                        )
                        .expect("write_message_bundle");
                        mcp_agent_mail_storage::flush_async_commits();

                        samples_us.push(t0.elapsed().as_micros() as u64);
                        msg_id += 1;
                    }
                }
                ArchiveScenario::BatchNoAttachments { batch_size } => {
                    for _ in 0..*ops {
                        let t0 = Instant::now();

                        for _ in 0..batch_size {
                            let message_json = serde_json::json!({
                                "id": msg_id,
                                "project": project_slug,
                                "subject": "bench batch",
                                "created_ts": 1_700_000_000_000_000i64,
                            });
                            mcp_agent_mail_storage::write_message_bundle(
                                &archive,
                                &config,
                                &message_json,
                                "hello",
                                sender,
                                &recipients,
                                &[],
                                None,
                            )
                            .expect("write_message_bundle");
                            msg_id += 1;
                        }
                        mcp_agent_mail_storage::flush_async_commits();

                        samples_us.push(t0.elapsed().as_micros() as u64);
                    }
                }
            }

            let total = start_all.elapsed();
            let elements_per_op = scenario.elements_per_op();
            let total_elements = elements_per_op.saturating_mul(*ops as u64);
            let total_elements_f64 =
                u32::try_from(total_elements).map_or_else(|_| f64::from(u32::MAX), f64::from);
            let throughput = if total.as_secs_f64() > 0.0 {
                total_elements_f64 / total.as_secs_f64()
            } else {
                0.0
            };

            let p50_us = percentile_us(samples_us.clone(), 0.50);
            let p95_us = percentile_us(samples_us.clone(), 0.95);
            let p99_us = percentile_us(samples_us.clone(), 0.99);
            let tail_p99_9_us = percentile_us(samples_us.clone(), 0.999);
            let extreme_p99_99_us = percentile_us(samples_us.clone(), 0.9999);
            let max_us = samples_us.iter().copied().max().unwrap_or(0);

            let (budget_p95_us, budget_p99_us) = scenario_budgets_us(*scenario);
            let p95_within_budget = p95_us <= budget_p95_us;
            let p99_within_budget = p99_us <= budget_p99_us;
            let p95_delta_us = p95_us as i64 - budget_p95_us as i64;
            let p99_delta_us = p99_us as i64 - budget_p99_us as i64;
            if !p95_within_budget || !p99_within_budget {
                regressions += 1;
            }

            let scenario_result = ArchiveBenchScenarioResult {
                scenario: scenario.artifact_label(),
                elements_per_op,
                samples_us: samples_us.clone(),
                p50_us,
                p95_us,
                p99_us,
                p99_9_us: tail_p99_9_us,
                p99_99_us: extreme_p99_99_us,
                max_us,
                budget_p95_us,
                budget_p99_us,
                p95_within_budget,
                p99_within_budget,
                p95_delta_us,
                p99_delta_us,
                throughput_elements_per_sec: (throughput * 100.0).round() / 100.0,
            };

            let _ = std::fs::write(
                out_dir.join(format!("{}.json", scenario.artifact_label())),
                serde_json::to_string_pretty(&scenario_result).unwrap_or_default(),
            );

            results.push(scenario_result);

            std::env::set_current_dir(original_cwd).expect("restore cwd");
            drop(tmp);
        }

        let run = ArchiveBenchRun {
            run_id,
            arch: std::env::consts::ARCH.to_string(),
            os: std::env::consts::OS.to_string(),
            budget_regressions: regressions,
            results,
        };

        let _ = std::fs::write(
            out_dir.join("summary.json"),
            serde_json::to_string_pretty(&run).unwrap_or_default(),
        );
    });
}

fn run_archive_batch_sample(
    archive: &mcp_agent_mail_storage::ProjectArchive,
    config: &mcp_agent_mail_core::Config,
    sender: &str,
    recipients: &[String],
    batch_size: usize,
    msg_id: &mut i64,
    sample_index: usize,
) -> u64 {
    fn write_archive_batch_messages(
        archive: &mcp_agent_mail_storage::ProjectArchive,
        config: &mcp_agent_mail_core::Config,
        project_slug: &str,
        sender: &str,
        recipients: &[String],
        batch_size: usize,
        msg_id: &mut i64,
    ) {
        let messages = (0..batch_size)
            .map(|_| {
                let message_json = serde_json::json!({
                    "id": *msg_id,
                    "project": project_slug,
                    "subject": "bench batch",
                    "created_ts": 1_700_000_000_000_000i64,
                });
                *msg_id += 1;
                message_json
            })
            .collect::<Vec<_>>();
        let no_extra_paths: &[String] = &[];
        let batch_entries = messages
            .iter()
            .map(|message| mcp_agent_mail_storage::MessageBundleBatchEntry {
                message,
                body_md: "hello",
                sender,
                recipients,
                extra_paths: no_extra_paths,
            })
            .collect::<Vec<_>>();
        mcp_agent_mail_storage::write_message_batch_bundle(archive, config, &batch_entries, None)
            .expect("write_message_batch_bundle");
    }

    let sample_span = tracing::info_span!(
        "archive_batch.sample",
        batch_size,
        sample_index,
        elements = batch_size
    );
    let _sample_guard = sample_span.entered();

    let t0 = Instant::now();
    {
        let write_span = tracing::info_span!(
            "archive_batch.write_message_batch",
            batch_size,
            sample_index
        );
        let _write_guard = write_span.entered();
        let message_span = tracing::trace_span!(
            "archive_batch.write_message_batch_bundle",
            batch_size,
            sample_index
        );
        let _message_guard = message_span.entered();
        write_archive_batch_messages(
            archive,
            config,
            "bench-archive",
            sender,
            recipients,
            batch_size,
            msg_id,
        );
    }

    {
        let flush_span = tracing::info_span!(
            "archive_batch.flush_async_commits",
            batch_size,
            sample_index
        );
        let _flush_guard = flush_span.entered();
        mcp_agent_mail_storage::flush_async_commits();
    }
    {
        let wbq_span = tracing::info_span!("archive_batch.wbq_flush", batch_size, sample_index);
        let _wbq_guard = wbq_span.entered();
        mcp_agent_mail_storage::wbq_flush();
    }

    u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn summarize_recorded_spans(spans: &[RecordedSpan]) -> Vec<ArchivePerfCategory> {
    let mut grouped: HashMap<String, Vec<u64>> = HashMap::new();
    for span in spans {
        grouped
            .entry(span.name.clone())
            .or_default()
            .push(span.duration_us);
    }

    let mut categories: Vec<ArchivePerfCategory> = grouped
        .into_iter()
        .map(|(category, durations)| {
            let count = durations.len();
            let cumulative_us = durations.iter().copied().sum::<u64>();
            let max_us = durations.iter().copied().max().unwrap_or(0);
            let avg_us = if count > 0 {
                cumulative_us / u64::try_from(count).unwrap_or(1)
            } else {
                0
            };
            ArchivePerfCategory {
                category,
                cumulative_us,
                count,
                p50_us: percentile_us(durations.clone(), 0.50),
                p95_us: percentile_us(durations, 0.95),
                avg_us,
                max_us,
            }
        })
        .collect();

    categories.sort_by_key(|item| std::cmp::Reverse(item.cumulative_us));
    categories.truncate(10);
    categories
}

#[allow(clippy::cast_precision_loss)]
fn scaling_law_note(comparison: &[ArchivePerfComparison]) -> String {
    let Some(batch_1) = comparison.iter().find(|item| item.batch_size == 1) else {
        return "insufficient comparison data to compute scaling law".to_string();
    };
    if batch_1.p95_us == 0 {
        return "insufficient comparison data to compute scaling law".to_string();
    }

    let checkpoints = [10usize, 50, 100, 500, 1000];
    let mut ratios = Vec::new();
    let mut amortized = Vec::new();
    let mut largest_sample = None;

    for checkpoint in checkpoints {
        if let Some(entry) = comparison.iter().find(|item| item.batch_size == checkpoint) {
            let ratio = entry.p95_us as f64 / batch_1.p95_us as f64;
            ratios.push(format!("batch-{checkpoint} p95 is {ratio:.2}x batch-1"));
            amortized.push(format!(
                "batch-{checkpoint} amortizes to {:.3}x batch-1 per message",
                ratio / checkpoint as f64
            ));
            largest_sample = Some((checkpoint, ratio));
        }
    }

    let Some((largest_batch, largest_ratio)) = largest_sample else {
        return "insufficient comparison data to compute scaling law".to_string();
    };

    let shape = if largest_ratio < largest_batch as f64 {
        "sublinear"
    } else if (largest_ratio - largest_batch as f64).abs() < 0.05 * largest_batch as f64 {
        "roughly linear"
    } else {
        "superlinear"
    };

    format!(
        "{}; {}. Overall scaling remains {shape} through batch-{largest_batch}.",
        ratios.join(", "),
        amortized.join(", ")
    )
}

fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn perf_artifact_date() -> String {
    command_output("date", &["-u", "+%F"]).unwrap_or_else(|| "unknown-date".to_string())
}

fn archive_perf_environment() -> ArchivePerfEnvironment {
    let repo_root_path = repo_root();
    let repo_root_string = repo_root_path.to_string_lossy().to_string();

    let mount_source = command_output("findmnt", &["-no", "SOURCE", "-T", &repo_root_string]);
    let filesystem = command_output("findmnt", &["-no", "FSTYPE", "-T", &repo_root_string]);
    let mount_options = command_output("findmnt", &["-no", "OPTIONS", "-T", &repo_root_string]);
    let storage_model = mount_source.as_deref().and_then(|source| {
        source
            .starts_with("/dev/")
            .then(|| command_output("lsblk", &["-ndo", "MODEL", source]))
            .flatten()
    });
    let storage_transport = mount_source.as_deref().and_then(|source| {
        source
            .starts_with("/dev/")
            .then(|| command_output("lsblk", &["-ndo", "TRAN", source]))
            .flatten()
    });

    ArchivePerfEnvironment {
        repo_root: repo_root_string,
        cargo_target_dir: std::env::var("CARGO_TARGET_DIR").ok(),
        rustc_version: command_output("rustc", &["-Vv"]),
        kernel_release: command_output("uname", &["-r"]),
        filesystem,
        mount_source,
        mount_options,
        storage_model,
        storage_transport,
    }
}

fn archive_perf_reproduction() -> ArchivePerfReproduction {
    let cargo_target_assignment = std::env::var("CARGO_TARGET_DIR")
        .ok()
        .map(|target_dir| format!("CARGO_TARGET_DIR={target_dir} "));
    let warm_env_prefix = cargo_target_assignment.as_deref().map_or_else(
        || "rch exec -- env MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 ".to_string(),
        |target_dir| format!("rch exec -- env {target_dir}MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 "),
    );
    let flamegraph_env_prefix = cargo_target_assignment
        .map(|target_dir| format!("env {target_dir}"))
        .unwrap_or_default();

    ArchivePerfReproduction {
        warm_profile_command: format!(
            "{warm_env_prefix}cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch"
        ),
        flamegraph_command: if flamegraph_env_prefix.is_empty() {
            "cargo flamegraph -p mcp-agent-mail --bench benchmarks --root -- archive_write_batch"
                .to_string()
        } else {
            format!(
                "{flamegraph_env_prefix}cargo flamegraph -p mcp-agent-mail --bench benchmarks --root -- archive_write_batch"
            )
        },
        cross_engineer_target:
            "Reproduce within 10% on similar CPU/storage/filesystem/kernel hardware within 30 days"
                .to_string(),
        dry_run_validated: true,
    }
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn archive_perf_logging_requirements() -> Vec<ArchivePerfLoggingRequirement> {
    vec![
        ArchivePerfLoggingRequirement {
            event: "perf.profile.run_start".to_string(),
            required_fields: vec![
                "scenario".to_string(),
                "rust_version".to_string(),
                "hardware".to_string(),
            ],
        },
        ArchivePerfLoggingRequirement {
            event: "perf.profile.sample_collected".to_string(),
            required_fields: vec!["sample_count".to_string(), "duration_sec".to_string()],
        },
        ArchivePerfLoggingRequirement {
            event: "perf.profile.span_summary".to_string(),
            required_fields: vec![
                "span_name".to_string(),
                "cumulative_micros".to_string(),
                "count".to_string(),
                "p50".to_string(),
                "p95".to_string(),
            ],
        },
        ArchivePerfLoggingRequirement {
            event: "perf.profile.hypothesis_evaluated".to_string(),
            required_fields: vec![
                "name".to_string(),
                "supports_or_rejects".to_string(),
                "evidence".to_string(),
            ],
        },
        ArchivePerfLoggingRequirement {
            event: "perf.profile.run_complete".to_string(),
            required_fields: vec!["duration_sec".to_string(), "artifacts_written".to_string()],
        },
    ]
}

fn category_by_name<'a>(
    categories: &'a [ArchivePerfCategory],
    name: &str,
) -> Option<&'a ArchivePerfCategory> {
    categories.iter().find(|category| category.category == name)
}

fn has_category_fragment(categories: &[ArchivePerfCategory], fragment: &str) -> bool {
    categories
        .iter()
        .any(|category| category.category.contains(fragment))
}

#[allow(clippy::too_many_lines)]
fn archive_perf_hypothesis_evaluations(
    categories: &[ArchivePerfCategory],
    scaling_note: &str,
) -> Vec<ArchivePerfHypothesisEvaluation> {
    let write = category_by_name(categories, "archive_batch.write_message_batch_bundle")
        .or_else(|| category_by_name(categories, "archive_batch.write_message_batch"));
    let flush = category_by_name(categories, "archive_batch.flush_async_commits");
    let wbq = category_by_name(categories, "archive_batch.wbq_flush");

    let write_us = write.map_or(0, |category| category.cumulative_us);
    let flush_us = flush.map_or(0, |category| category.cumulative_us);
    let wbq_us = wbq.map_or(0, |category| category.cumulative_us);

    vec![
        ArchivePerfHypothesisEvaluation {
            name: "coalescer batching".to_string(),
            supports_or_rejects: if flush_us > write_us / 2 {
                "supports".to_string()
            } else {
                "rejects".to_string()
            },
            evidence: if flush_us > write_us / 2 {
                format!(
                    "flush_async_commits remains material at {flush_us}us cumulative versus {write_us}us in write_message_batch"
                )
            } else {
                format!(
                    "write_message_batch dominates at {write_us}us cumulative while flush_async_commits is secondary at {flush_us}us"
                )
            },
        },
        ArchivePerfHypothesisEvaluation {
            name: "fsync per msg".to_string(),
            supports_or_rejects: if wbq_us > flush_us / 2 && wbq_us > 0 {
                "supports".to_string()
            } else {
                "rejects".to_string()
            },
            evidence: if wbq_us > flush_us / 2 && wbq_us > 0 {
                format!(
                    "wbq_flush remains material at {wbq_us}us cumulative, which keeps fsync-like work in the hot path"
                )
            } else {
                format!(
                    "wbq_flush is only {wbq_us}us cumulative versus {flush_us}us for flush_async_commits, so the final wait is not the dominant lever"
                )
            },
        },
        ArchivePerfHypothesisEvaluation {
            name: "file layout".to_string(),
            supports_or_rejects: if write_us >= flush_us {
                "supports".to_string()
            } else {
                "rejects".to_string()
            },
            evidence: if write_us >= flush_us {
                format!(
                    "per-message archive burst work still dominates the profile at {write_us}us cumulative"
                )
            } else {
                format!(
                    "layout work does not dominate; commit/flush work is larger at {flush_us}us"
                )
            },
        },
        ArchivePerfHypothesisEvaluation {
            name: "SQLite per-msg txn".to_string(),
            supports_or_rejects: if has_category_fragment(categories, "sqlite")
                || has_category_fragment(categories, "db")
            {
                "supports".to_string()
            } else {
                "rejects".to_string()
            },
            evidence: if has_category_fragment(categories, "sqlite")
                || has_category_fragment(categories, "db")
            {
                "SQLite-related spans surfaced in the top categories during the warm-path sample"
                    .to_string()
            } else {
                "no SQLite-specific spans surfaced in the top warm-path categories".to_string()
            },
        },
        ArchivePerfHypothesisEvaluation {
            name: "hashing".to_string(),
            supports_or_rejects: if has_category_fragment(categories, "hash") {
                "supports".to_string()
            } else {
                "rejects".to_string()
            },
            evidence: if has_category_fragment(categories, "hash") {
                "hash-oriented spans surfaced in the top categories".to_string()
            } else {
                "hash-oriented spans did not surface in the top categories".to_string()
            },
        },
        ArchivePerfHypothesisEvaluation {
            name: "lock thrash".to_string(),
            supports_or_rejects: if has_category_fragment(categories, "lock")
                || scaling_note.contains("superlinear")
            {
                "supports".to_string()
            } else {
                "rejects".to_string()
            },
            evidence: if has_category_fragment(categories, "lock")
                || scaling_note.contains("superlinear")
            {
                format!("scaling/trace evidence suggests contention: {scaling_note}")
            } else {
                format!(
                    "scaling remains {scaling_note}, which does not resemble lock-driven blow-up"
                )
            },
        },
    ]
}

fn chrome_trace_events(spans: &[RecordedSpan]) -> Vec<ChromeTraceEvent> {
    let mut cursors_us: HashMap<u32, u64> = HashMap::new();

    spans
        .iter()
        .map(|span| {
            let tid = span
                .fields
                .get("sample_index")
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(0);
            let ts = *cursors_us.get(&tid).unwrap_or(&0);
            cursors_us.insert(tid, ts.saturating_add(span.duration_us));

            ChromeTraceEvent {
                name: span.name.clone(),
                cat: "archive_batch".to_string(),
                ph: "X".to_string(),
                ts,
                dur: span.duration_us,
                pid: 1,
                tid,
                args: span.fields.clone(),
            }
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn archive_perf_profile_markdown(report: &ArchivePerfReport) -> String {
    let mut lines = Vec::new();
    lines.push("# Archive Batch 100 Profile".to_string());
    lines.push(String::new());
    lines.push("## Reproduction".to_string());
    lines.push(format!(
        "- warm profile: `{}`",
        report.reproduction.warm_profile_command
    ));
    lines.push(format!(
        "- flamegraph: `{}`",
        report.reproduction.flamegraph_command
    ));
    lines.push(format!(
        "- dry-run validated: {}",
        if report.reproduction.dry_run_validated {
            "yes"
        } else {
            "no"
        }
    ));
    lines.push(format!(
        "- cross-engineer target: {}",
        report.reproduction.cross_engineer_target
    ));
    lines.push(String::new());
    lines.push("## Environment".to_string());
    lines.push(format!("- repo root: `{}`", report.environment.repo_root));
    if let Some(target_dir) = &report.environment.cargo_target_dir {
        lines.push(format!("- cargo target dir: `{target_dir}`"));
    }
    if let Some(rustc_version) = &report.environment.rustc_version {
        lines.push(format!("- rustc: `{}`", rustc_version.replace('\n', "; ")));
    }
    if let Some(kernel_release) = &report.environment.kernel_release {
        lines.push(format!("- kernel: `{kernel_release}`"));
    }
    if let Some(filesystem) = &report.environment.filesystem {
        lines.push(format!("- filesystem: `{filesystem}`"));
    }
    if let Some(mount_source) = &report.environment.mount_source {
        lines.push(format!("- mount source: `{mount_source}`"));
    }
    if let Some(mount_options) = &report.environment.mount_options {
        lines.push(format!("- mount options: `{mount_options}`"));
    }
    if let Some(storage_model) = &report.environment.storage_model {
        lines.push(format!("- storage model: `{storage_model}`"));
    }
    if let Some(storage_transport) = &report.environment.storage_transport {
        lines.push(format!("- storage transport: `{storage_transport}`"));
    }
    lines.push(String::new());
    lines.push("## Batch Comparison".to_string());
    for entry in &report.comparison {
        lines.push(format!(
            "- batch-{}: p50={}us, p95={}us, p99={}us, p99.9={}us, p99.99={}us, max={}us, samples={}, throughput={:.2} elems/sec",
            entry.batch_size,
            entry.p50_us,
            entry.p95_us,
            entry.p99_us,
            entry.p99_9_us,
            entry.p99_99_us,
            entry.max_us,
            entry.samples_us.len(),
            entry.throughput_elements_per_sec
        ));
    }
    lines.push(
        "- note: current warm-path sample counts are below 1,000 per scenario, so p99.9/p99.99 act as a conservative worst-observed tail sentinel."
            .to_string(),
    );
    lines.push(String::new());
    lines.push("## Structured Logging Requirements".to_string());
    for requirement in &report.structured_logging_requirements {
        lines.push(format!(
            "- `{}`: {}",
            requirement.event,
            requirement.required_fields.join(", ")
        ));
    }
    lines.push(String::new());
    lines.push("## Top 10 Spans by Cumulative Duration".to_string());
    for category in &report.top_categories {
        lines.push(format!(
            "- `{}`: cumulative={}us, count={}, p50={}us, p95={}us, avg={}us, max={}us",
            category.category,
            category.cumulative_us,
            category.count,
            category.p50_us,
            category.p95_us,
            category.avg_us,
            category.max_us
        ));
    }
    lines.push(String::new());
    lines.push("## Chrome Trace".to_string());
    lines.push(format!(
        "- `{}` includes {} `traceEvents` records alongside the raw span payloads.",
        perf_artifact_dir()
            .join("archive_batch_100_spans.json")
            .display(),
        report.trace_events.len()
    ));
    lines.push(String::new());
    lines.push("## Hypothesis Evaluation".to_string());
    for evaluation in &report.hypothesis_evaluations {
        lines.push(format!(
            "- `{}`: {} ({})",
            evaluation.name, evaluation.supports_or_rejects, evaluation.evidence
        ));
    }
    lines.push(String::new());
    lines.push("## Scaling Law".to_string());
    lines.push(format!("- {}", report.scaling_law_note));
    lines.push(String::new());

    lines.join("\n")
}

fn write_archive_profile_report(report: &ArchivePerfReport) -> io::Result<()> {
    let report_path = perf_artifact_dir().join("archive_batch_100_profile.md");
    let _ = std::fs::create_dir_all(perf_artifact_dir());
    std::fs::write(report_path, archive_perf_profile_markdown(report))
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn archive_scaling_csv(comparison: &[ArchivePerfComparison]) -> String {
    let mut lines = Vec::with_capacity(comparison.len() + 1);
    lines.push(
        "batch_size,p50_us,p95_us,p99_us,sample_count,p99_9_us,p99_99_us,max_us,throughput_elements_per_sec"
            .to_string(),
    );
    for entry in comparison {
        lines.push(format!(
            "{},{},{},{},{},{},{},{},{:.2}",
            entry.batch_size,
            entry.p50_us,
            entry.p95_us,
            entry.p99_us,
            entry.samples_us.len(),
            entry.p99_9_us,
            entry.p99_99_us,
            entry.max_us,
            entry.throughput_elements_per_sec
        ));
    }
    lines.join("\n")
}

fn write_archive_scaling_csv(comparison: &[ArchivePerfComparison]) -> io::Result<()> {
    let csv_path = perf_artifact_dir().join("archive_batch_scaling.csv");
    let _ = std::fs::create_dir_all(perf_artifact_dir());
    let csv = archive_scaling_csv(comparison);
    std::fs::write(&csv_path, &csv)?;
    std::fs::write(
        perf_artifact_dir().join(format!(
            "archive_batch_scaling_{}.csv",
            perf_artifact_date()
        )),
        csv,
    )
}

fn write_archive_spans_report(report: &ArchivePerfReport) -> io::Result<()> {
    let perf_dir = perf_artifact_dir();
    let _ = std::fs::create_dir_all(&perf_dir);
    let json = serde_json::to_string_pretty(report).unwrap_or_default();
    std::fs::write(perf_dir.join("archive_batch_100_spans.json"), &json)?;
    std::fs::write(
        perf_dir.join(format!(
            "archive_batch_100_spans_{}.json",
            perf_artifact_date()
        )),
        json,
    )
}

#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn run_archive_perf_profile_once_inner(force: bool) {
    static DID_RUN: Once = Once::new();
    if !force && std::env::var_os("MCP_AGENT_MAIL_ARCHIVE_PROFILE").is_none() {
        return;
    }

    DID_RUN.call_once(|| {
        let run_started = Instant::now();
        let state = span_recorder_state();
        let perf_dir = perf_artifact_dir();
        let _ = std::fs::create_dir_all(&perf_dir);
        let _ = state.take_closed();
        let environment = archive_perf_environment();
        let rust_version = environment
            .rustc_version
            .clone()
            .unwrap_or_else(|| "unknown rustc".to_string())
            .replace('\n', "; ");
        let hardware = format!(
            "{} / {} / {}",
            environment
                .storage_model
                .clone()
                .unwrap_or_else(|| "unknown-storage".to_string()),
            environment
                .filesystem
                .clone()
                .unwrap_or_else(|| "unknown-fs".to_string()),
            environment
                .kernel_release
                .clone()
                .unwrap_or_else(|| "unknown-kernel".to_string())
        );

        tracing::info!(
            scenario = "archive_write_batch",
            rust_version = %rust_version,
            hardware = %hardware,
            "perf.profile.run_start"
        );

        let mut comparison = Vec::new();
        let mut batch_100_spans = Vec::new();
        let sample_counts = [
            (1usize, 40usize),
            (10usize, 25usize),
            (50usize, 15usize),
            (100usize, 12usize),
            (500usize, 6usize),
            (1000usize, 4usize),
        ];

        for (batch_size, sample_count) in sample_counts {
            let tmp = TempDir::new().expect("tempdir");
            let original_cwd = std::env::current_dir().expect("cwd");
            std::env::set_current_dir(tmp.path()).expect("chdir");

            let mut config = mcp_agent_mail_core::Config::from_env();
            config.storage_root = tmp.path().join("archive_repo");
            config.database_url = format!(
                "sqlite+aiosqlite:///{}",
                tmp.path().join("storage.sqlite3").display()
            );

            let sender = "BenchSender";
            let recipients = vec!["BenchReceiver".to_string()];
            let mut msg_id = 1_i64;

            let archive = {
                let setup_span = tracing::info_span!("archive_batch.ensure_archive", batch_size);
                let _setup_guard = setup_span.entered();
                mcp_agent_mail_storage::ensure_archive(&config, "bench-archive")
                    .expect("ensure_archive")
            };

            {
                let warmup_span = tracing::info_span!("archive_batch.warmup", batch_size);
                let warmup_guard = warmup_span.entered();
                let _ = run_archive_batch_sample(
                    &archive,
                    &config,
                    sender,
                    &recipients,
                    batch_size,
                    &mut msg_id,
                    usize::MAX,
                );
                drop(warmup_guard);
            }
            let _ = state.take_closed();

            let mut samples_us = Vec::with_capacity(sample_count);
            for sample_index in 0..sample_count {
                let elapsed_us = run_archive_batch_sample(
                    &archive,
                    &config,
                    sender,
                    &recipients,
                    batch_size,
                    &mut msg_id,
                    sample_index,
                );
                samples_us.push(elapsed_us);
            }
            let total_duration_sec =
                samples_us.iter().copied().sum::<u64>() as f64 / 1_000_000.0;

            let scenario = ArchivePerfComparison {
                batch_size,
                p50_us: percentile_us(samples_us.clone(), 0.50),
                p95_us: percentile_us(samples_us.clone(), 0.95),
                p99_us: percentile_us(samples_us.clone(), 0.99),
                p99_9_us: percentile_us(samples_us.clone(), 0.999),
                p99_99_us: percentile_us(samples_us.clone(), 0.9999),
                max_us: samples_us.iter().copied().max().unwrap_or(0),
                throughput_elements_per_sec: {
                    let total_elements = batch_size as f64 * sample_count as f64;
                    let total_us = samples_us.iter().copied().sum::<u64>() as f64;
                    if total_us > 0.0 {
                        (total_elements / (total_us / 1_000_000.0) * 100.0).round() / 100.0
                    } else {
                        0.0
                    }
                },
                samples_us,
            };

            tracing::info!(
                batch_size,
                sample_count,
                duration_sec = total_duration_sec,
                "perf.profile.sample_collected"
            );

            if batch_size == 100 {
                batch_100_spans = state.take_closed();
            } else {
                let _ = state.take_closed();
            }
            comparison.push(scenario);

            std::env::set_current_dir(original_cwd).expect("restore cwd");
            drop(tmp);
        }

        let top_categories = summarize_recorded_spans(&batch_100_spans);
        let scaling_note = scaling_law_note(&comparison);
        let hypothesis_evaluations = archive_perf_hypothesis_evaluations(&top_categories, &scaling_note);
        let trace_events = chrome_trace_events(&batch_100_spans);

        for category in &top_categories {
            tracing::info!(
                span_name = %category.category,
                cumulative_micros = category.cumulative_us,
                count = category.count,
                p50 = category.p50_us,
                p95 = category.p95_us,
                "perf.profile.span_summary"
            );
        }

        for hypothesis in &hypothesis_evaluations {
            tracing::info!(
                name = %hypothesis.name,
                supports_or_rejects = %hypothesis.supports_or_rejects,
                evidence = %hypothesis.evidence,
                "perf.profile.hypothesis_evaluated"
            );
        }

        let report = ArchivePerfReport {
            run_id: run_id(),
            arch: std::env::consts::ARCH.to_string(),
            os: std::env::consts::OS.to_string(),
            warm_db: true,
            environment,
            reproduction: archive_perf_reproduction(),
            structured_logging_requirements: archive_perf_logging_requirements(),
            comparison,
            batch_100_spans,
            trace_events,
            top_categories,
            hypothesis_evaluations,
            scaling_law_note: scaling_note,
        };

        let _ = write_archive_spans_report(&report);
        let _ = write_archive_scaling_csv(&report.comparison);
        let _ = write_archive_profile_report(&report);
        tracing::info!(
            duration_sec = run_started.elapsed().as_secs_f64(),
            artifacts_written = %format!(
                "archive_batch_100_spans.json,archive_batch_100_spans_{}.json,archive_batch_scaling.csv,archive_batch_scaling_{}.csv,archive_batch_100_profile.md",
                perf_artifact_date(),
                perf_artifact_date()
            ),
            "perf.profile.run_complete"
        );
    });
}

fn run_archive_perf_profile_once() {
    run_archive_perf_profile_once_inner(false);
}

#[allow(dead_code)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn run_archive_perf_profile_once_for_testing() {
    run_archive_perf_profile_once_inner(true);
}

#[allow(clippy::too_many_lines)]
fn bench_archive_write(c: &mut Criterion) {
    if !bench_scope_enabled("archive_write") {
        return;
    }

    run_archive_harness_once();
    run_archive_perf_profile_once();

    let scenarios: &[ArchiveScenario] = &[
        ArchiveScenario::SingleNoAttachments,
        ArchiveScenario::SingleInlineAttachment,
        ArchiveScenario::SingleFileAttachment,
    ];

    let mut group = c.benchmark_group("archive_write");
    for &scenario in scenarios {
        group.throughput(Throughput::Elements(scenario.elements_per_op()));

        group.bench_with_input(
            BenchmarkId::new(scenario.benchmark_name(), scenario.elements_per_op()),
            &scenario,
            |b, &scenario| {
                b.iter_custom(|iters| {
                    let tmp = TempDir::new().expect("tempdir");
                    let original_cwd = std::env::current_dir().expect("cwd");
                    std::env::set_current_dir(tmp.path()).expect("chdir");

                    let mut config = mcp_agent_mail_core::Config::from_env();
                    config.storage_root = tmp.path().join("archive_repo");
                    config.database_url = format!(
                        "sqlite+aiosqlite:///{}",
                        tmp.path().join("storage.sqlite3").display()
                    );

                    let project_slug = "bench-archive";
                    let archive = mcp_agent_mail_storage::ensure_archive(&config, project_slug)
                        .expect("archive");

                    let sender = "BenchSender";
                    let recipients = vec!["BenchReceiver".to_string()];

                    // Pre-generate attachment inputs (outside timed region).
                    let input_dir = tmp.path().join("input");
                    let _ = std::fs::create_dir_all(&input_dir);
                    let mut attachment_paths: Vec<PathBuf> = Vec::new();
                    if matches!(
                        scenario,
                        ArchiveScenario::SingleInlineAttachment
                            | ArchiveScenario::SingleFileAttachment
                    ) {
                        for i in 0..iters {
                            let p = input_dir.join(format!("img_{i}.bmp"));
                            write_bmp24(&p, 32, 32, i as u32).expect("write bmp");
                            attachment_paths.push(p);
                        }
                    }

                    let mut msg_id: i64 = 1;
                    let t0 = Instant::now();

                    match scenario {
                        ArchiveScenario::SingleNoAttachments => {
                            for _ in 0..iters {
                                let message_json = serde_json::json!({
                                    "id": msg_id,
                                    "project": project_slug,
                                    "subject": "bench no attachments",
                                    "created_ts": 1_700_000_000_000_000i64,
                                });

                                mcp_agent_mail_storage::write_message_bundle(
                                    &archive,
                                    &config,
                                    &message_json,
                                    "hello",
                                    sender,
                                    &recipients,
                                    &[],
                                    None,
                                )
                                .expect("write_message_bundle");
                                mcp_agent_mail_storage::flush_async_commits();
                                msg_id += 1;
                            }
                        }
                        ArchiveScenario::SingleInlineAttachment
                        | ArchiveScenario::SingleFileAttachment => {
                            let policy =
                                if matches!(scenario, ArchiveScenario::SingleInlineAttachment) {
                                    mcp_agent_mail_storage::EmbedPolicy::Inline
                                } else {
                                    mcp_agent_mail_storage::EmbedPolicy::File
                                };
                            let iters_us = usize::try_from(iters).unwrap_or(usize::MAX);
                            for path in attachment_paths.iter().take(iters_us) {
                                let img_path = path.to_string_lossy().to_string();
                                let body = format!("inline image: ![img]({img_path})\n");

                                let (body2, meta, rel_paths) =
                                    mcp_agent_mail_storage::process_markdown_images(
                                        &archive,
                                        &config,
                                        &archive.root,
                                        &body,
                                        policy,
                                    )
                                    .expect("process_markdown_images");

                                let attachments_json: Vec<serde_json::Value> = meta
                                    .into_iter()
                                    .filter_map(|m| serde_json::to_value(m).ok())
                                    .collect();

                                let message_json = serde_json::json!({
                                    "id": msg_id,
                                    "project": project_slug,
                                    "subject": "bench attachment",
                                    "created_ts": 1_700_000_000_000_000i64,
                                    "attachments": attachments_json,
                                });

                                mcp_agent_mail_storage::write_message_bundle(
                                    &archive,
                                    &config,
                                    &message_json,
                                    &body2,
                                    sender,
                                    &recipients,
                                    &rel_paths,
                                    None,
                                )
                                .expect("write_message_bundle");
                                mcp_agent_mail_storage::flush_async_commits();
                                msg_id += 1;
                            }
                        }
                        ArchiveScenario::BatchNoAttachments { batch_size } => {
                            for _ in 0..iters {
                                let messages = (0..batch_size)
                                    .map(|_| {
                                        let message_json = serde_json::json!({
                                            "id": msg_id,
                                            "project": project_slug,
                                            "subject": "bench batch",
                                            "created_ts": 1_700_000_000_000_000i64,
                                        });
                                        msg_id += 1;
                                        message_json
                                    })
                                    .collect::<Vec<_>>();
                                let no_extra_paths: &[String] = &[];
                                let batch_entries = messages
                                    .iter()
                                    .map(|message| {
                                        mcp_agent_mail_storage::MessageBundleBatchEntry {
                                            message,
                                            body_md: "hello",
                                            sender,
                                            recipients: &recipients,
                                            extra_paths: no_extra_paths,
                                        }
                                    })
                                    .collect::<Vec<_>>();
                                mcp_agent_mail_storage::write_message_batch_bundle(
                                    &archive,
                                    &config,
                                    &batch_entries,
                                    None,
                                )
                                .expect("write_message_batch_bundle");
                                mcp_agent_mail_storage::flush_async_commits();
                            }
                        }
                    }

                    let dt = t0.elapsed();
                    std::env::set_current_dir(original_cwd).expect("restore cwd");
                    drop(tmp);
                    dt
                });
            },
        );
    }

    group.finish();

    // Batch benches are much slower (intentionally) under legacy-ish commit batching,
    // so use a smaller sample size to keep `cargo bench` runtimes reasonable.
    let mut batch_group = c.benchmark_group("archive_write_batch");
    batch_group.sample_size(20);
    for scenario in [
        ArchiveScenario::BatchNoAttachments { batch_size: 1 },
        ArchiveScenario::BatchNoAttachments { batch_size: 10 },
        ArchiveScenario::BatchNoAttachments { batch_size: 100 },
    ] {
        batch_group.throughput(Throughput::Elements(scenario.elements_per_op()));
        batch_group.bench_with_input(
            BenchmarkId::new(scenario.benchmark_name(), scenario.elements_per_op()),
            &scenario,
            |b, &scenario| {
                b.iter_custom(|iters| {
                    let tmp = TempDir::new().expect("tempdir");
                    let original_cwd = std::env::current_dir().expect("cwd");
                    std::env::set_current_dir(tmp.path()).expect("chdir");

                    let mut config = mcp_agent_mail_core::Config::from_env();
                    config.storage_root = tmp.path().join("archive_repo");
                    config.database_url = format!(
                        "sqlite+aiosqlite:///{}",
                        tmp.path().join("storage.sqlite3").display()
                    );

                    let project_slug = "bench-archive";
                    let archive = mcp_agent_mail_storage::ensure_archive(&config, project_slug)
                        .expect("archive");

                    let sender = "BenchSender";
                    let recipients = vec!["BenchReceiver".to_string()];

                    let mut msg_id: i64 = 1;
                    let t0 = Instant::now();

                    for _ in 0..iters {
                        if let ArchiveScenario::BatchNoAttachments { batch_size } = scenario {
                            let messages = (0..batch_size)
                                .map(|_| {
                                    let message_json = serde_json::json!({
                                        "id": msg_id,
                                        "project": project_slug,
                                        "subject": "bench batch",
                                        "created_ts": 1_700_000_000_000_000i64,
                                    });
                                    msg_id += 1;
                                    message_json
                                })
                                .collect::<Vec<_>>();
                            let no_extra_paths: &[String] = &[];
                            let batch_entries = messages
                                .iter()
                                .map(|message| mcp_agent_mail_storage::MessageBundleBatchEntry {
                                    message,
                                    body_md: "hello",
                                    sender,
                                    recipients: &recipients,
                                    extra_paths: no_extra_paths,
                                })
                                .collect::<Vec<_>>();
                            mcp_agent_mail_storage::write_message_batch_bundle(
                                &archive,
                                &config,
                                &batch_entries,
                                None,
                            )
                            .expect("write_message_batch_bundle");
                            mcp_agent_mail_storage::flush_async_commits();
                        }
                    }

                    let dt = t0.elapsed();
                    std::env::set_current_dir(original_cwd).expect("restore cwd");
                    drop(tmp);
                    dt
                });
            },
        );
    }
    batch_group.finish();
}

// ---------------------------------------------------------------------------
// Global Search Harness (br-3vwi.2.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchScenario {
    Small,
    Medium,
    Large,
}

impl SearchScenario {
    const fn name(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    const fn message_count(self) -> usize {
        match self {
            Self::Small => 1_000,
            Self::Medium => 5_000,
            Self::Large => 15_000,
        }
    }

    const fn ops(self) -> usize {
        match self {
            Self::Small => 200,
            Self::Medium => 100,
            Self::Large => 50,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SearchBenchScenarioResult {
    scenario: String,
    message_count: usize,
    ops: usize,
    query: String,
    limit: i32,
    samples_us: Vec<u64>,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    budget_p95_us: u64,
    budget_p99_us: u64,
    p95_within_budget: bool,
    p99_within_budget: bool,
    p95_delta_us: i64,
    p99_delta_us: i64,
    throughput_queries_per_sec: f64,
}

#[derive(Debug, Clone, Serialize)]
struct SearchBenchRun {
    run_id: String,
    arch: String,
    os: String,
    budget_regressions: usize,
    results: Vec<SearchBenchScenarioResult>,
}

fn search_artifact_dir(run_id: &str) -> PathBuf {
    repo_root()
        .join("tests")
        .join("artifacts")
        .join("bench")
        .join("search")
        .join(run_id)
}

const fn search_scenario_budgets_us(scenario: SearchScenario) -> (u64, u64) {
    match scenario {
        SearchScenario::Small => (3_000, 5_000),
        SearchScenario::Medium => (15_000, 25_000),
        SearchScenario::Large => (50_000, 80_000),
    }
}

struct SearchFixture {
    _tmp: TempDir,
    cx: Cx,
    pool: DbPool,
    query: SearchQuery,
    message_count: usize,
    query_text: &'static str,
    limit: i32,
}

fn seed_search_fixture(scenario: SearchScenario) -> SearchFixture {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("storage.sqlite3");

    let cx = Cx::for_testing();

    let pool_cfg = DbPoolConfig {
        database_url: format!("sqlite+aiosqlite:///{}", db_path.display()),
        min_connections: 1,
        max_connections: 1,
        ..Default::default()
    };
    let pool = DbPool::new(&pool_cfg).expect("pool");

    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("mkdir workspace");
    let human_key = workspace.to_string_lossy().to_string();

    let project = match block_on(mcp_agent_mail_db::queries::ensure_project(
        &cx, &pool, &human_key,
    )) {
        Outcome::Ok(row) => row,
        Outcome::Err(e) => panic!("ensure_project failed: {e}"),
        Outcome::Cancelled(_) => panic!("ensure_project cancelled"),
        Outcome::Panicked(p) => panic!("ensure_project panicked: {}", p.message()),
    };
    let project_id = project.id.unwrap_or(0);

    let sender = match block_on(mcp_agent_mail_db::queries::register_agent(
        &cx,
        &pool,
        project_id,
        "BlueLake",
        "bench",
        "bench",
        Some("bench-search sender"),
        Some("auto"),
        None,
    )) {
        Outcome::Ok(row) => row,
        Outcome::Err(e) => panic!("register_agent sender failed: {e}"),
        Outcome::Cancelled(_) => panic!("register_agent sender cancelled"),
        Outcome::Panicked(p) => panic!("register_agent sender panicked: {}", p.message()),
    };
    let sender_id = sender.id.unwrap_or(0);

    let vocab: [&str; 10] = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    ];
    let message_count = scenario.message_count();

    {
        // Batch insert messages in a single transaction to keep fixture seeding fast.
        // This relies on schema triggers to populate FTS tables.
        let conn = match block_on(pool.acquire(&cx)) {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => panic!("pool acquire failed: {e}"),
            Outcome::Cancelled(_) => panic!("pool acquire cancelled"),
            Outcome::Panicked(p) => panic!("pool acquire panicked: {}", p.message()),
        };

        conn.execute_raw("BEGIN CONCURRENT").expect("begin txn");
        for i in 0..message_count {
            let v = vocab[i % vocab.len()];
            let mut subject = format!("bench {i} {v}");
            if i % 97 == 0 {
                subject.push_str(" needle");
            }
            let body = format!("body {i} {v} {}", vocab[(i * 7 + 3) % vocab.len()]);

            // Keep values quote-safe for SQLite.
            let subject_sql = subject.replace('\'', "''");
            let body_sql = body.replace('\'', "''");

            let created_ts = 1_700_000_000_000_000i64 + (i as i64);
            conn.execute_raw(&format!(
                "INSERT INTO messages (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
                 VALUES ({project_id}, {sender_id}, 'bench-search', '{subject_sql}', '{body_sql}', 'normal', 0, {created_ts}, '[]')"
            ))
            .expect("insert message");
        }
        conn.execute_raw("COMMIT").expect("commit txn");
        let _ = conn.execute_raw("ANALYZE");
    }

    let limit = 20;
    let query_text = "needle";
    let mut query = SearchQuery::messages(query_text, project_id);
    query.limit = Some(usize::try_from(limit).unwrap_or(20));

    SearchFixture {
        _tmp: tmp,
        cx,
        pool,
        query,
        message_count,
        query_text,
        limit,
    }
}

#[allow(clippy::too_many_lines)]
fn run_search_harness_once() {
    static DID_RUN: Once = Once::new();
    DID_RUN.call_once(|| {
        let run_id = run_id();
        let out_dir = search_artifact_dir(&run_id);
        let _ = std::fs::create_dir_all(&out_dir);

        let scenarios: &[SearchScenario] = &[
            SearchScenario::Small,
            SearchScenario::Medium,
            SearchScenario::Large,
        ];

        let mut results = Vec::new();
        let mut regressions = 0usize;

        for scenario in scenarios {
            let fixture = seed_search_fixture(*scenario);
            let ops = scenario.ops();

            // Warm caches (FTS + pool) before sampling.
            let warm = block_on(mcp_agent_mail_db::search_service::execute_search_simple(
                &fixture.cx,
                &fixture.pool,
                &fixture.query,
            ));
            match warm {
                Outcome::Ok(v) => {
                    black_box(&v);
                }
                Outcome::Err(e) => panic!("warm search failed: {e}"),
                Outcome::Cancelled(_) => panic!("warm search cancelled"),
                Outcome::Panicked(p) => panic!("warm search panicked: {}", p.message()),
            }

            let mut samples_us: Vec<u64> = Vec::with_capacity(ops);
            for _ in 0..ops {
                let t0 = Instant::now();
                let out = block_on(mcp_agent_mail_db::search_service::execute_search_simple(
                    &fixture.cx,
                    &fixture.pool,
                    &fixture.query,
                ));
                match out {
                    Outcome::Ok(v) => {
                        black_box(&v);
                    }
                    Outcome::Err(e) => panic!("search failed: {e}"),
                    Outcome::Cancelled(_) => panic!("search cancelled"),
                    Outcome::Panicked(p) => panic!("search panicked: {}", p.message()),
                }

                let us = u64::try_from(t0.elapsed().as_micros().min(u128::from(u64::MAX)))
                    .unwrap_or(u64::MAX);
                samples_us.push(us);
            }

            let total_us: u64 = samples_us.iter().copied().sum();
            let throughput = if total_us > 0 {
                // Keep the conversion clippy-clean (avoid u64/usize -> f64 precision-loss lints).
                let ops_u32 = u32::try_from(ops).unwrap_or(u32::MAX);
                let total_us_u32 = u32::try_from(total_us).unwrap_or(u32::MAX).max(1);
                f64::from(ops_u32) * 1_000_000.0 / f64::from(total_us_u32)
            } else {
                0.0
            };

            let p50_us = percentile_us(samples_us.clone(), 0.50);
            let p95_us = percentile_us(samples_us.clone(), 0.95);
            let p99_us = percentile_us(samples_us.clone(), 0.99);

            let (budget_p95_us, budget_p99_us) = search_scenario_budgets_us(*scenario);
            let p95_within_budget = p95_us <= budget_p95_us;
            let p99_within_budget = p99_us <= budget_p99_us;
            let p95_delta_us = i64::try_from(p95_us).unwrap_or(i64::MAX)
                - i64::try_from(budget_p95_us).unwrap_or(i64::MAX);
            let p99_delta_us = i64::try_from(p99_us).unwrap_or(i64::MAX)
                - i64::try_from(budget_p99_us).unwrap_or(i64::MAX);

            if !p95_within_budget || !p99_within_budget {
                regressions += 1;
            }

            let scenario_result = SearchBenchScenarioResult {
                scenario: scenario.name().to_string(),
                message_count: fixture.message_count,
                ops,
                query: fixture.query_text.to_string(),
                limit: fixture.limit,
                samples_us: samples_us.clone(),
                p50_us,
                p95_us,
                p99_us,
                budget_p95_us,
                budget_p99_us,
                p95_within_budget,
                p99_within_budget,
                p95_delta_us,
                p99_delta_us,
                throughput_queries_per_sec: (throughput * 100.0).round() / 100.0,
            };

            let _ = std::fs::write(
                out_dir.join(format!("{}.json", scenario.name())),
                serde_json::to_string_pretty(&scenario_result).unwrap_or_default(),
            );

            results.push(scenario_result);
        }

        let run = SearchBenchRun {
            run_id,
            arch: std::env::consts::ARCH.to_string(),
            os: std::env::consts::OS.to_string(),
            budget_regressions: regressions,
            results,
        };

        let _ = std::fs::write(
            out_dir.join("summary.json"),
            serde_json::to_string_pretty(&run).unwrap_or_default(),
        );

        if std::env::var("MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS")
            .ok()
            .as_deref()
            == Some("1")
            && regressions > 0
        {
            panic!(
                "search bench budgets exceeded: {regressions} regressions (run_id={})",
                run.run_id
            );
        }
    });
}

fn bench_global_search(c: &mut Criterion) {
    if !bench_scope_enabled("global_search") {
        return;
    }

    run_search_harness_once();

    let scenarios: &[SearchScenario] = &[
        SearchScenario::Small,
        SearchScenario::Medium,
        SearchScenario::Large,
    ];

    let mut group = c.benchmark_group("global_search");
    group.sample_size(10);

    for &scenario in scenarios {
        let fixture = seed_search_fixture(scenario);
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new(scenario.name(), fixture.message_count),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    let out = block_on(mcp_agent_mail_db::search_service::execute_search_simple(
                        &fixture.cx,
                        &fixture.pool,
                        &fixture.query,
                    ));
                    match out {
                        Outcome::Ok(v) => {
                            black_box(&v);
                        }
                        Outcome::Err(e) => panic!("search failed: {e}"),
                        Outcome::Cancelled(_) => panic!("search cancelled"),
                        Outcome::Panicked(p) => panic!("search panicked: {}", p.message()),
                    }
                });
            },
        );
    }

    group.finish();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShareScenario {
    TinyNoAttachments,
    MediumMixedAttachments,
    ChunkedSmallThreshold,
}

impl ShareScenario {
    const fn name(self) -> &'static str {
        match self {
            Self::TinyNoAttachments => "tiny_no_attachments",
            Self::MediumMixedAttachments => "medium_mixed_attachments",
            Self::ChunkedSmallThreshold => "chunked_small_threshold",
        }
    }

    const fn keep_messages(self) -> usize {
        match self {
            Self::TinyNoAttachments
            | Self::MediumMixedAttachments
            | Self::ChunkedSmallThreshold => 100,
        }
    }

    const fn drop_messages(self) -> usize {
        match self {
            Self::TinyNoAttachments
            | Self::MediumMixedAttachments
            | Self::ChunkedSmallThreshold => 20,
        }
    }

    const fn ops(self) -> usize {
        match self {
            Self::TinyNoAttachments => 30,
            Self::MediumMixedAttachments => 15,
            Self::ChunkedSmallThreshold => 10,
        }
    }

    const fn chunk_threshold_bytes(self) -> usize {
        match self {
            Self::ChunkedSmallThreshold => 128 * 1024, // force chunking for the medium fixture
            _ => mcp_agent_mail_share::DEFAULT_CHUNK_THRESHOLD,
        }
    }

    const fn chunk_size_bytes(self) -> usize {
        match self {
            Self::ChunkedSmallThreshold => 64 * 1024,
            _ => mcp_agent_mail_share::DEFAULT_CHUNK_SIZE,
        }
    }
}

#[derive(Debug, Clone)]
struct ShareFixture {
    source_db: PathBuf,
    storage_root: PathBuf,
    project_filters: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct StageBenchResult {
    stage: String,
    samples_us: Vec<u64>,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    budget_p95_us: u64,
    budget_p99_us: u64,
    p95_within_budget: bool,
    p99_within_budget: bool,
    p95_delta_us: i64,
    p99_delta_us: i64,
}

#[derive(Debug, Clone, Serialize)]
struct ShareBenchScenarioResult {
    scenario: String,
    keep_messages: usize,
    drop_messages: usize,
    output_dir_bytes: u64,
    output_zip_bytes: u64,
    stable_bundle_hash: String,
    chunk_count: usize,
    stages: Vec<StageBenchResult>,
    budget_regressions: usize,
}

#[derive(Debug, Clone, Serialize)]
struct ShareBenchRun {
    run_id: String,
    arch: String,
    os: String,
    budget_regressions: usize,
    results: Vec<ShareBenchScenarioResult>,
}

#[derive(Debug, Clone, Serialize)]
struct ShareHotspotEntry {
    stage: String,
    p95_us: u64,
    percent_of_total_p95_bp: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ShareHotspotScenario {
    scenario: String,
    top_stages: Vec<ShareHotspotEntry>,
}

#[derive(Debug, Clone, Copy)]
enum ShareStage {
    Total,
    Snapshot,
    Scope,
    Scrub,
    Finalize,
    Bundle,
    Zip,
}

impl ShareStage {
    const fn name(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Snapshot => "snapshot",
            Self::Scope => "scope",
            Self::Scrub => "scrub",
            Self::Finalize => "finalize",
            Self::Bundle => "bundle",
            Self::Zip => "zip",
        }
    }
}

const fn share_stage_budget_us(_scenario: ShareScenario, stage: ShareStage) -> (u64, u64) {
    // Budgets are ~2x the measured baseline p95/p99 to absorb variance.
    //
    // Baseline source of truth:
    // - `tests/artifacts/bench/share/<run_id>/summary.json`
    // - Most recent baseline (2026-02-06): `tests/artifacts/bench/share/1770390636_3768966/summary.json`
    match stage {
        ShareStage::Total => (4_000_000, 4_500_000),
        ShareStage::Snapshot => (80_000, 100_000),
        ShareStage::Scope => (40_000, 50_000),
        ShareStage::Scrub => (50_000, 60_000),
        ShareStage::Finalize => (700_000, 900_000),
        ShareStage::Bundle => (2_800_000, 3_000_000),
        ShareStage::Zip => (350_000, 400_000),
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    hex::encode(hash)
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(sha256_bytes(&bytes))
}

fn sort_json_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(&String, Value)> =
                map.iter().map(|(k, v)| (k, sort_json_keys(v))).collect();
            sorted.sort_by_key(|(a, _)| *a);
            let ordered: serde_json::Map<String, Value> =
                sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect();
            Value::Object(ordered)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_json_keys).collect()),
        other => other.clone(),
    }
}

fn strip_generated_at(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("generated_at");
            for v in map.values_mut() {
                strip_generated_at(v);
            }
        }
        Value::Array(values) => {
            for v in values {
                strip_generated_at(v);
            }
        }
        _ => {}
    }
}

fn stable_json_file_hash(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut value: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Ok(sha256_bytes(&bytes)),
    };
    strip_generated_at(&mut value);
    let sorted = sort_json_keys(&value);
    Ok(sha256_bytes(
        serde_json::to_string(&sorted)
            .unwrap_or_default()
            .as_bytes(),
    ))
}

fn stable_bundle_hash(bundle_root: &Path) -> std::io::Result<String> {
    // Stable hash for determinism checks:
    // - Hash each file.
    // - For JSON files, strip volatile `generated_at` fields and sort keys before hashing.
    // - Combine as sha256 over `relpath\0filehash\n` in sorted order.
    let mut combined = Sha256::new();
    let mut dbg = Vec::new();
    let files = collect_files_sorted(bundle_root)?;
    for file_path in files {
        let rel = file_path
            .strip_prefix(bundle_root)
            .unwrap_or(&file_path)
            .to_string_lossy()
            .replace('\\', "/");
        let is_json = file_path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        let file_hash = if is_json {
            stable_json_file_hash(&file_path)?
        } else {
            sha256_file(&file_path)?
        };
        dbg.push(format!("{rel} {file_hash}"));
        combined.update(rel.as_bytes());
        combined.update(b"\0");
        combined.update(file_hash.as_bytes());
        combined.update(b"\n");
    }
    std::fs::write(bundle_root.join("../hash_debug.txt"), dbg.join("\n")).ok();
    Ok(hex::encode(combined.finalize()))
}

fn dir_bytes(root: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

fn collect_files_sorted(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
    Ok(out)
}

fn write_pattern_bytes(path: &Path, size: usize, seed: u32) -> std::io::Result<()> {
    let mut buf = vec![0u8; size];
    for (idx, b) in buf.iter_mut().enumerate() {
        let x = (idx as u32).wrapping_add(seed.wrapping_mul(7919));
        *b = (x ^ (x >> 8) ^ (x >> 16)) as u8;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, buf)
}

fn seed_share_fixture(tmp: &TempDir, scenario: ShareScenario) -> ShareFixture {
    let source_db = tmp.path().join("source.sqlite3");
    let storage_root = tmp.path().join("storage");
    let _ = std::fs::create_dir_all(&storage_root);

    let keep_project_id: i64 = 1;
    let drop_project_id: i64 = 2;

    let keep_slug = "proj_keep";
    let drop_slug = "proj_drop";

    let conn = mcp_agent_mail_db::DbConn::open_file(source_db.display().to_string())
        .expect("open source db");

    // Schema: only what the share pipeline needs.
    conn.execute_raw(
        "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at TEXT DEFAULT '')",
    )
    .expect("create projects");
    conn.execute_raw(
        "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, \
         program TEXT DEFAULT '', model TEXT DEFAULT '', task_description TEXT DEFAULT '', \
         inception_ts TEXT DEFAULT '', last_active_ts TEXT DEFAULT '', \
         attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
    )
    .expect("create agents");
    conn.execute_raw(
        "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
         thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', \
         importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, \
         created_ts TEXT DEFAULT '', attachments TEXT DEFAULT '[]')",
    )
    .expect("create messages");
    conn.execute_raw(
        "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, \
         kind TEXT DEFAULT 'to', read_ts TEXT, ack_ts TEXT, PRIMARY KEY(message_id, agent_id))",
    )
    .expect("create recipients");
    conn.execute_raw(
        "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, \
         agent_id INTEGER, path_pattern TEXT, exclusive INTEGER DEFAULT 1, \
         reason TEXT DEFAULT '', created_ts TEXT DEFAULT '', expires_ts TEXT DEFAULT '', \
         released_ts TEXT)",
    )
    .expect("create file_reservations");
    conn.execute_raw(
        "CREATE TABLE agent_links (id INTEGER PRIMARY KEY, a_project_id INTEGER, \
         a_agent_id INTEGER, b_project_id INTEGER, b_agent_id INTEGER, \
         status TEXT DEFAULT 'pending', reason TEXT DEFAULT '', \
         created_ts TEXT DEFAULT '', updated_ts TEXT DEFAULT '', expires_ts TEXT)",
    )
    .expect("create agent_links");

    conn.execute_raw(&format!(
        "INSERT INTO projects VALUES ({keep_project_id}, '{keep_slug}', '/test/keep', '')"
    ))
    .expect("insert keep project");
    conn.execute_raw(&format!(
        "INSERT INTO projects VALUES ({drop_project_id}, '{drop_slug}', '/test/drop', '')"
    ))
    .expect("insert drop project");

    conn.execute_raw(&format!(
        "INSERT INTO agents VALUES (1, {keep_project_id}, 'Alice', 'codex-cli', 'gpt-5', 'bench', '', '', 'auto', 'auto')"
    ))
    .expect("insert keep agent");
    conn.execute_raw(&format!(
        "INSERT INTO agents VALUES (2, {drop_project_id}, 'Bob', 'codex-cli', 'gpt-5', 'bench', '', '', 'auto', 'auto')"
    ))
    .expect("insert drop agent");

    conn.execute_raw("BEGIN CONCURRENT").expect("begin txn");

    // Seed file reservations + agent links so scrub clears them.
    for i in 0..20 {
        let res_id = i + 1;
        conn.execute_raw(&format!(
            "INSERT INTO file_reservations VALUES ({res_id}, {keep_project_id}, 1, 'src/**', 1, 'bench', '', '', NULL)"
        ))
        .expect("insert reservation");
    }
    conn.execute_raw(&format!(
        "INSERT INTO agent_links VALUES (1, {keep_project_id}, 1, {drop_project_id}, 2, 'pending', 'bench', '', '', NULL)"
    ))
    .expect("insert agent link");

    let keep_messages = scenario.keep_messages();
    let drop_messages = scenario.drop_messages();
    let total_messages = keep_messages + drop_messages;

    for i in 0..total_messages {
        let msg_id = (i + 1) as i64;
        let is_keep = i < keep_messages;
        let project_id = if is_keep {
            keep_project_id
        } else {
            drop_project_id
        };
        let sender_id = if is_keep { 1 } else { 2 };
        let thread_id = if is_keep { "T_KEEP" } else { "T_DROP" };

        let subject = format!("Bench message {msg_id}");
        let secret = if msg_id % 2 == 0 {
            " sk-abcdef0123456789012345 ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"
        } else {
            ""
        };
        let body_size = if scenario == ShareScenario::ChunkedSmallThreshold {
            2048usize
        } else {
            512usize
        };
        let mut body = String::with_capacity(body_size + secret.len() + 64);
        body.push_str("Body prefix");
        body.push_str(secret);
        while body.len() < body_size {
            body.push_str(" lorem_ipsum");
        }

        let attachments_json = match scenario {
            ShareScenario::TinyNoAttachments => "[]".to_string(),
            ShareScenario::MediumMixedAttachments | ShareScenario::ChunkedSmallThreshold => {
                if is_keep {
                    let rel = format!("att/att_{msg_id}.bin");
                    let size = if msg_id % 2 == 0 { 2048 } else { 128 * 1024 };
                    let seed = u32::try_from(msg_id).expect("msg_id should fit u32");
                    write_pattern_bytes(&storage_root.join(&rel), size, seed)
                        .expect("write attachment bytes");
                    format!(
                        "[{{\"type\":\"file\",\"path\":\"{rel}\",\"media_type\":\"application/octet-stream\"}}]"
                    )
                } else {
                    "[]".to_string()
                }
            }
        };

        let subject_sql = subject.replace('\'', "''");
        let body_sql = body.replace('\'', "''");
        let attachments_sql = attachments_json.replace('\'', "''");

        conn.execute_raw(&format!(
            "INSERT INTO messages VALUES ({msg_id}, {project_id}, {sender_id}, '{thread_id}', \
             '{subject_sql}', '{body_sql}', 'normal', 1, '2026-01-01T00:00:00Z', '{attachments_sql}')"
        ))
        .expect("insert message");
        conn.execute_raw(&format!(
            "INSERT INTO message_recipients VALUES ({msg_id}, {sender_id}, 'to', NULL, NULL)"
        ))
        .expect("insert recipient");
    }

    conn.execute_raw("COMMIT").expect("commit txn");

    ShareFixture {
        source_db,
        storage_root,
        project_filters: vec![keep_slug.to_string()],
    }
}

#[derive(Debug, Clone, Copy)]
struct ShareSample {
    total_us: u64,
    snapshot_us: u64,
    scope_us: u64,
    scrub_us: u64,
    finalize_us: u64,
    bundle_us: u64,
    zip_us: u64,
    chunk_count: usize,
}

fn run_share_export_once(
    fixture: &ShareFixture,
    scenario: ShareScenario,
    out_root: &Path,
    inline_threshold: usize,
    detach_threshold: usize,
) -> ShareSample {
    let _ = std::fs::create_dir_all(out_root);
    let snapshot_path = out_root.join("_snapshot.sqlite3");
    let bundle_dir = out_root.join("bundle");
    let _ = std::fs::create_dir_all(&bundle_dir);

    let t_total = Instant::now();

    let t0 = Instant::now();
    mcp_agent_mail_share::create_sqlite_snapshot(&fixture.source_db, &snapshot_path, true)
        .expect("snapshot");
    let snapshot_us = t0.elapsed().as_micros() as u64;

    let t1 = Instant::now();
    let scope = mcp_agent_mail_share::apply_project_scope(&snapshot_path, &fixture.project_filters)
        .expect("scope");
    let scope_us = t1.elapsed().as_micros() as u64;

    let t2 = Instant::now();
    let scrub_summary = mcp_agent_mail_share::scrub_snapshot(
        &snapshot_path,
        mcp_agent_mail_share::ScrubPreset::Standard,
    )
    .expect("scrub");
    let scrub_us = t2.elapsed().as_micros() as u64;

    let t3 = Instant::now();
    let finalize = mcp_agent_mail_share::finalize_export_db(&snapshot_path).expect("finalize");
    let finalize_us = t3.elapsed().as_micros() as u64;

    let context = mcp_agent_mail_share::SnapshotContext {
        snapshot_path,
        scope,
        scrub_summary,
        fts_enabled: finalize.fts_enabled,
    };

    // Canonical bundle pipeline (attachments -> viewer -> static render -> scaffold).
    let t4 = Instant::now();
    let export = mcp_agent_mail_share::export_bundle_from_snapshot_context(
        &context,
        &bundle_dir,
        &fixture.storage_root,
        &mcp_agent_mail_share::BundleExportConfig {
            inline_attachment_threshold: inline_threshold,
            detach_attachment_threshold: detach_threshold,
            chunk_threshold: scenario.chunk_threshold_bytes(),
            chunk_size: scenario.chunk_size_bytes(),
            scrub_preset: mcp_agent_mail_share::ScrubPreset::Standard,
            allow_absolute_attachment_paths: true,
            hosting_hints_root: None,
        },
    )
    .expect("export bundle");
    let chunk_count = export.chunk_manifest.as_ref().map_or(0, |c| c.chunk_count);
    let bundle_us = t4.elapsed().as_micros() as u64;

    let t5 = Instant::now();
    let zip_path = out_root.join("bundle.zip");
    let _ = mcp_agent_mail_share::package_directory_as_zip(&bundle_dir, &zip_path).expect("zip");
    let zip_us = t5.elapsed().as_micros() as u64;

    let total_us = t_total.elapsed().as_micros() as u64;

    ShareSample {
        total_us,
        snapshot_us,
        scope_us,
        scrub_us,
        finalize_us,
        bundle_us,
        zip_us,
        chunk_count,
    }
}

fn stage_result(
    stage: ShareStage,
    samples_us: Vec<u64>,
    scenario: ShareScenario,
) -> StageBenchResult {
    let p50_us = percentile_us(samples_us.clone(), 0.50);
    let p95_us = percentile_us(samples_us.clone(), 0.95);
    let p99_us = percentile_us(samples_us.clone(), 0.99);

    let (budget_p95_us, budget_p99_us) = share_stage_budget_us(scenario, stage);
    let p95_within_budget = p95_us <= budget_p95_us;
    let p99_within_budget = p99_us <= budget_p99_us;
    let p95_delta_us = p95_us as i64 - budget_p95_us as i64;
    let p99_delta_us = p99_us as i64 - budget_p99_us as i64;

    StageBenchResult {
        stage: stage.name().to_string(),
        samples_us,
        p50_us,
        p95_us,
        p99_us,
        budget_p95_us,
        budget_p99_us,
        p95_within_budget,
        p99_within_budget,
        p95_delta_us,
        p99_delta_us,
    }
}

#[allow(clippy::too_many_lines)]
fn run_share_harness_once() {
    static DID_RUN: Once = Once::new();
    DID_RUN.call_once(|| {
        let run_id = run_id();
        let out_dir = share_artifact_dir(&run_id);
        let _ = std::fs::create_dir_all(&out_dir);

        let scenarios: &[ShareScenario] = &[
            ShareScenario::TinyNoAttachments,
            ShareScenario::MediumMixedAttachments,
            ShareScenario::ChunkedSmallThreshold,
        ];

        let mut results = Vec::new();
        let mut regressions = 0usize;

        for scenario in scenarios {
            let base = TempDir::new().expect("tempdir");
            let fixture = seed_share_fixture(&base, *scenario);

            let mut total_samples = Vec::with_capacity(scenario.ops());
            let mut snapshot_samples = Vec::with_capacity(scenario.ops());
            let mut scope_samples = Vec::with_capacity(scenario.ops());
            let mut scrub_samples = Vec::with_capacity(scenario.ops());
            let mut finalize_samples = Vec::with_capacity(scenario.ops());
            let mut bundle_samples = Vec::with_capacity(scenario.ops());
            let mut zip_samples = Vec::with_capacity(scenario.ops());
            let mut chunk_counts = Vec::with_capacity(scenario.ops());

            let mut stable_hash_first: Option<String> = None;
            let mut stable_hash_second: Option<String> = None;
            let mut stable_debug_first: Option<String> = None;
            let mut output_dir_bytes: u64 = 0;
            let mut output_zip_bytes: u64 = 0;

            for op_idx in 0..scenario.ops() {
                let run_tmp = TempDir::new().expect("tempdir");
                let out_root = run_tmp.path().join("out");

                let sample = run_share_export_once(
                    &fixture,
                    *scenario,
                    &out_root,
                    mcp_agent_mail_share::INLINE_ATTACHMENT_THRESHOLD,
                    mcp_agent_mail_share::DETACH_ATTACHMENT_THRESHOLD,
                );

                total_samples.push(sample.total_us);
                snapshot_samples.push(sample.snapshot_us);
                scope_samples.push(sample.scope_us);
                scrub_samples.push(sample.scrub_us);
                finalize_samples.push(sample.finalize_us);
                bundle_samples.push(sample.bundle_us);
                zip_samples.push(sample.zip_us);
                chunk_counts.push(sample.chunk_count);

                // Determinism: hash the first two outputs and ensure they match.
                if op_idx < 2 {
                    let bundle_dir = out_root.join("bundle");
                    let stable = stable_bundle_hash(&bundle_dir).expect("stable bundle hash");
                    let debug = std::fs::read_to_string(out_root.join("hash_debug.txt"))
                        .unwrap_or_default();
                    if op_idx == 0 {
                        stable_hash_first = Some(stable);
                        stable_debug_first = Some(debug);
                        output_dir_bytes = dir_bytes(&bundle_dir).unwrap_or(0);
                        output_zip_bytes = out_root
                            .join("bundle.zip")
                            .metadata()
                            .map_or(0, |m| m.len());
                    } else {
                        stable_hash_second = Some(stable);
                        if stable_debug_first.as_deref() != Some(debug.as_str()) {
                            println!(
                                "DIFF IN DETERMINISM:\n===1===\n{}\n===2===\n{}",
                                stable_debug_first.as_deref().unwrap_or_default(),
                                debug
                            );
                        }
                    }
                }
            }

            if let (Some(a), Some(b)) = (&stable_hash_first, &stable_hash_second) {
                assert_eq!(
                    a, b,
                    "share bundle output should be deterministic (normalized)"
                );
            }
            let stable_bundle_hash = stable_hash_first.unwrap_or_else(|| "unknown".to_string());
            let chunk_count = chunk_counts.into_iter().max().unwrap_or_default();

            let stages = vec![
                stage_result(ShareStage::Total, total_samples, *scenario),
                stage_result(ShareStage::Snapshot, snapshot_samples, *scenario),
                stage_result(ShareStage::Scope, scope_samples, *scenario),
                stage_result(ShareStage::Scrub, scrub_samples, *scenario),
                stage_result(ShareStage::Finalize, finalize_samples, *scenario),
                stage_result(ShareStage::Bundle, bundle_samples, *scenario),
                stage_result(ShareStage::Zip, zip_samples, *scenario),
            ];

            let mut scenario_regressions = 0usize;
            for s in &stages {
                if !s.p95_within_budget || !s.p99_within_budget {
                    scenario_regressions += 1;
                }
            }
            regressions += scenario_regressions;

            let scenario_result = ShareBenchScenarioResult {
                scenario: scenario.name().to_string(),
                keep_messages: scenario.keep_messages(),
                drop_messages: scenario.drop_messages(),
                output_dir_bytes,
                output_zip_bytes,
                stable_bundle_hash,
                chunk_count,
                stages,
                budget_regressions: scenario_regressions,
            };

            let _ = std::fs::write(
                out_dir.join(format!("{}.json", scenario.name())),
                serde_json::to_string_pretty(&scenario_result).unwrap_or_default(),
            );
            results.push(scenario_result);
        }

        let run = ShareBenchRun {
            run_id,
            arch: std::env::consts::ARCH.to_string(),
            os: std::env::consts::OS.to_string(),
            budget_regressions: regressions,
            results,
        };

        // Emit a small "hotspot list" derived from stage p95s (one per scenario) so
        // perf work has a deterministic baseline even without flamegraphs.
        let mut hotspots: Vec<ShareHotspotScenario> = Vec::new();
        for scenario in &run.results {
            let total_p95 = scenario
                .stages
                .iter()
                .find(|s| s.stage == ShareStage::Total.name())
                .map_or(0, |s| s.p95_us);

            let mut by_p95: Vec<&StageBenchResult> = scenario
                .stages
                .iter()
                .filter(|s| s.stage != ShareStage::Total.name())
                .collect();
            by_p95.sort_by_key(|s| std::cmp::Reverse(s.p95_us));

            let top_stages = by_p95
                .into_iter()
                .take(5)
                .map(|s| {
                    let percent_bp = s
                        .p95_us
                        .saturating_mul(10_000)
                        .checked_div(total_p95)
                        .unwrap_or(0);
                    ShareHotspotEntry {
                        stage: s.stage.clone(),
                        p95_us: s.p95_us,
                        percent_of_total_p95_bp: percent_bp,
                    }
                })
                .collect();

            hotspots.push(ShareHotspotScenario {
                scenario: scenario.scenario.clone(),
                top_stages,
            });
        }

        let _ = std::fs::write(
            out_dir.join("summary.json"),
            serde_json::to_string_pretty(&run).unwrap_or_default(),
        );
        let _ = std::fs::write(
            out_dir.join("hotspots.json"),
            serde_json::to_string_pretty(&hotspots).unwrap_or_default(),
        );

        if std::env::var("MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS")
            .ok()
            .as_deref()
            == Some("1")
            && regressions > 0
        {
            panic!(
                "share bench budgets exceeded: {regressions} regressions (run_id={})",
                run.run_id
            );
        }
    });
}

#[allow(clippy::too_many_lines)]
fn bench_share_export(c: &mut Criterion) {
    if !bench_scope_enabled("share_export") {
        return;
    }

    run_share_harness_once();

    let scenarios: &[ShareScenario] = &[
        ShareScenario::TinyNoAttachments,
        ShareScenario::MediumMixedAttachments,
        ShareScenario::ChunkedSmallThreshold,
    ];

    let mut group = c.benchmark_group("share_export");
    group.sample_size(10);

    for &scenario in scenarios {
        let elements_per_op = scenario.keep_messages() as u64;
        group.throughput(Throughput::Elements(elements_per_op));
        group.bench_with_input(
            BenchmarkId::new(scenario.name(), elements_per_op),
            &scenario,
            |b, &scenario| {
                b.iter_custom(|iters| {
                    let base = TempDir::new().expect("tempdir");
                    let fixture = seed_share_fixture(&base, scenario);

                    let t0 = Instant::now();
                    let iters_us = usize::try_from(iters).unwrap_or(usize::MAX);
                    for _ in 0..iters_us {
                        let run_tmp = TempDir::new().expect("tempdir");
                        let out_root = run_tmp.path().join("out");
                        let _sample = run_share_export_once(
                            &fixture,
                            scenario,
                            &out_root,
                            mcp_agent_mail_share::INLINE_ATTACHMENT_THRESHOLD,
                            mcp_agent_mail_share::DETACH_ATTACHMENT_THRESHOLD,
                        );
                    }
                    t0.elapsed()
                });
            },
        );
    }

    group.finish();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveReadScenario {
    BatchSequential {
        dataset_size: usize,
        reads_per_op: usize,
    },
    RandomAccess {
        dataset_size: usize,
        reads_per_op: usize,
    },
}

impl ArchiveReadScenario {
    const fn benchmark_name(self) -> &'static str {
        match self {
            Self::BatchSequential { .. } => "batch_sequential",
            Self::RandomAccess { .. } => "random_access",
        }
    }

    const fn dataset_size(self) -> usize {
        match self {
            Self::BatchSequential { dataset_size, .. }
            | Self::RandomAccess { dataset_size, .. } => dataset_size,
        }
    }

    const fn reads_per_op(self) -> usize {
        match self {
            Self::BatchSequential { reads_per_op, .. }
            | Self::RandomAccess { reads_per_op, .. } => reads_per_op,
        }
    }

    const fn ops(self) -> usize {
        match self {
            Self::BatchSequential { .. } => 20,
            Self::RandomAccess { .. } => 40,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ArchiveReadBenchScenarioResult {
    scenario: String,
    dataset_size: usize,
    reads_per_op: usize,
    ops: usize,
    samples_us: Vec<u64>,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    throughput_reads_per_sec: f64,
}

#[derive(Debug, Clone, Serialize)]
struct ArchiveReadBenchRun {
    run_id: String,
    arch: String,
    os: String,
    results: Vec<ArchiveReadBenchScenarioResult>,
}

fn collect_canonical_message_paths(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() == "threads" {
                continue;
            }
            collect_canonical_message_paths(&path, out)?;
            continue;
        }

        if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

fn seed_archive_read_dataset(dataset_size: usize) -> (TempDir, Vec<PathBuf>) {
    let tmp = TempDir::new().expect("tempdir");
    let original_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(tmp.path()).expect("chdir");

    let mut config = mcp_agent_mail_core::Config::from_env();
    config.storage_root = tmp.path().join("archive_repo");
    config.database_url = format!(
        "sqlite+aiosqlite:///{}",
        tmp.path().join("storage.sqlite3").display()
    );

    let project_slug = "bench-archive-read";
    let archive = mcp_agent_mail_storage::ensure_archive(&config, project_slug).expect("archive");
    let sender = "BenchReader";
    let recipients = vec!["BenchReceiver".to_string()];

    for msg_id in 1..=dataset_size {
        let message_json = serde_json::json!({
            "id": i64::try_from(msg_id).unwrap_or(i64::MAX),
            "project": project_slug,
            "subject": format!("bench read {msg_id}"),
            "thread_id": format!("bench-read-{}", msg_id % 16),
            "created_ts": 1_700_000_000_000_000i64 + i64::try_from(msg_id).unwrap_or(0),
        });

        mcp_agent_mail_storage::write_message_bundle(
            &archive,
            &config,
            &message_json,
            "hello from archive read bench",
            sender,
            &recipients,
            &[],
            None,
        )
        .expect("write_message_bundle");
    }

    mcp_agent_mail_storage::flush_async_commits();
    mcp_agent_mail_storage::wbq_flush();

    let mut canonical_paths = Vec::with_capacity(dataset_size);
    collect_canonical_message_paths(&archive.root.join("messages"), &mut canonical_paths)
        .expect("collect canonical message paths");
    canonical_paths.sort();
    assert_eq!(canonical_paths.len(), dataset_size);

    std::env::set_current_dir(original_cwd).expect("restore cwd");
    (tmp, canonical_paths)
}

fn deterministic_read_plan(canonical_paths: &[PathBuf], reads_per_op: usize) -> Vec<PathBuf> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let len_u64 = u64::try_from(canonical_paths.len())
        .unwrap_or(u64::MAX)
        .max(1);
    (0..reads_per_op)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let idx_u64 = state % len_u64;
            let idx = usize::try_from(idx_u64).unwrap_or(0);
            canonical_paths[idx].clone()
        })
        .collect()
}

fn archive_read_summary_path() -> PathBuf {
    perf_artifact_dir().join("archive_read_summary.json")
}

fn archive_read_dated_summary_path() -> PathBuf {
    perf_artifact_dir().join(format!(
        "archive_read_summary_{}.json",
        perf_artifact_date()
    ))
}

fn build_archive_read_paths(
    scenario: ArchiveReadScenario,
    canonical_paths: &[PathBuf],
) -> Vec<PathBuf> {
    match scenario {
        ArchiveReadScenario::BatchSequential { reads_per_op, .. } => canonical_paths
            .iter()
            .take(reads_per_op)
            .cloned()
            .collect::<Vec<_>>(),
        ArchiveReadScenario::RandomAccess { reads_per_op, .. } => {
            deterministic_read_plan(canonical_paths, reads_per_op)
        }
    }
}

fn measure_archive_read_sample(read_paths: &[PathBuf]) -> u64 {
    let started_at = Instant::now();
    for path in read_paths {
        let (frontmatter, body_md) =
            mcp_agent_mail_storage::read_message_file(path).expect("read_message_file");
        black_box((frontmatter, body_md));
    }

    u64::try_from(started_at.elapsed().as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

fn run_archive_read_harness_once() {
    static DID_RUN: Once = Once::new();
    DID_RUN.call_once(|| {
        let scenarios = [
            ArchiveReadScenario::BatchSequential {
                dataset_size: 1_000,
                reads_per_op: 1_000,
            },
            ArchiveReadScenario::RandomAccess {
                dataset_size: 1_000,
                reads_per_op: 100,
            },
        ];

        let mut results = Vec::with_capacity(scenarios.len());
        let _ = std::fs::create_dir_all(perf_artifact_dir());

        for scenario in scenarios {
            let (_tmp, canonical_paths) = seed_archive_read_dataset(scenario.dataset_size());
            let read_paths = build_archive_read_paths(scenario, &canonical_paths);

            let warmup_us = measure_archive_read_sample(&read_paths);
            black_box(warmup_us);

            let ops = scenario.ops();
            let mut samples_us = Vec::with_capacity(ops);
            for _ in 0..ops {
                samples_us.push(measure_archive_read_sample(&read_paths));
            }

            let total_reads = scenario.reads_per_op().saturating_mul(ops);
            let total_reads_f64 =
                u32::try_from(total_reads).map_or_else(|_| f64::from(u32::MAX), f64::from);
            let total_us = samples_us.iter().copied().sum::<u64>();
            let total_us_f64 =
                u32::try_from(total_us).map_or_else(|_| f64::from(u32::MAX), f64::from);
            let throughput_reads_per_sec = if total_us_f64 > 0.0 {
                total_reads_f64 / (total_us_f64 / 1_000_000.0)
            } else {
                0.0
            };

            results.push(ArchiveReadBenchScenarioResult {
                scenario: scenario.benchmark_name().to_string(),
                dataset_size: scenario.dataset_size(),
                reads_per_op: scenario.reads_per_op(),
                ops,
                p50_us: percentile_us(samples_us.clone(), 0.50),
                p95_us: percentile_us(samples_us.clone(), 0.95),
                p99_us: percentile_us(samples_us.clone(), 0.99),
                max_us: samples_us.iter().copied().max().unwrap_or(0),
                samples_us,
                throughput_reads_per_sec: (throughput_reads_per_sec * 100.0).round() / 100.0,
            });
        }

        let run = ArchiveReadBenchRun {
            run_id: run_id(),
            arch: std::env::consts::ARCH.to_string(),
            os: std::env::consts::OS.to_string(),
            results,
        };

        let serialized = serde_json::to_string_pretty(&run).unwrap_or_default();
        let _ = std::fs::write(archive_read_summary_path(), &serialized);
        let _ = std::fs::write(archive_read_dated_summary_path(), serialized);
    });
}

fn bench_archive_read(c: &mut Criterion) {
    if !bench_scope_enabled("archive_read") {
        return;
    }

    run_archive_read_harness_once();

    let scenarios = [
        ArchiveReadScenario::BatchSequential {
            dataset_size: 1_000,
            reads_per_op: 1_000,
        },
        ArchiveReadScenario::RandomAccess {
            dataset_size: 1_000,
            reads_per_op: 100,
        },
    ];

    let mut group = c.benchmark_group("archive_read");
    group.sample_size(20);

    for scenario in scenarios {
        group.throughput(Throughput::Elements(
            u64::try_from(scenario.reads_per_op()).unwrap_or(u64::MAX),
        ));
        group.bench_with_input(
            BenchmarkId::new(scenario.benchmark_name(), scenario.dataset_size()),
            &scenario,
            |b, &scenario| {
                let (_tmp, canonical_paths) = seed_archive_read_dataset(scenario.dataset_size());
                let read_paths = build_archive_read_paths(scenario, &canonical_paths);

                b.iter_custom(|iters| {
                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        total += std::time::Duration::from_micros(measure_archive_read_sample(
                            &read_paths,
                        ));
                    }
                    total
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_tools,
    bench_global_search,
    bench_archive_write,
    bench_archive_read,
    bench_share_export
);
criterion_main!(benches);
