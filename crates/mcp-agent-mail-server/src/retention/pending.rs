//! Replay durable closeout intents without requiring the original client to return.
//!
//! ACK and release replay each own a worker, scan state, and lazy database pool.
//! An owned supervisor replaces exited workers with capped retry backoff. A
//! replacement reopens the durable journal; it never inherits a failed cursor.
//! A blocked release archive write must not stop acknowledgement recovery (or
//! vice versa). Shared database locks can still delay both kinds of mutation.
//! The workers are independent of destructive retention. Scans admit no mutation
//! before a complete, handle-validated snapshot. A step consumes at most 256 KiB or
//! 256 records; snapshots, retained identities and pending payloads have explicit
//! admission bounds. At most sixteen acknowledgements and sixteen release pages
//! of up to 64 candidate rows follow per pass. These are row/work bounds, not
//! hard deadlines on filesystem I/O or SQLite's internal query execution.
//! Failed intents remain queued without another failure record every tick.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use asupersync::{Cx, Outcome};
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{
    DbError, DbPool, DbPoolConfig, IdempotencyClaim, IdempotentOutcome, queries,
};
use mcp_agent_mail_tools::degraded_intents::{
    self as journal, QueuedAckIntent, QueuedReleaseIntentView,
};
use mcp_agent_mail_tools::tool_util::{resolve_agent, resolve_existing_project};
use serde_json::json;

mod journal_scan;
mod releases;
mod supervision;

const MAX_ATTEMPTS: usize = 16;
const POLL_INTERVAL: Duration = Duration::from_secs(30);
const CATCH_UP_INTERVAL: Duration = Duration::from_secs(1);
const SCAN_INTERVAL: Duration = Duration::from_millis(100);
type ReplayWorkerSlots = [Option<ReplayWorker>; 2];
static WORKERS: LazyLock<Mutex<Option<supervision::Service>>> = LazyLock::new(|| Mutex::new(None));
const REPLAY_KINDS: [journal_scan::Kind; 2] =
    [journal_scan::Kind::Ack, journal_scan::Kind::Release];

type IntentKey = (i64, String);

/// One finite round has a fixed upper bound. New arrivals cannot keep a failed
/// old intent behind a perpetually advancing tail. Full hashes disambiguate
/// equal timestamps and the journal's short diagnostic IDs.
#[derive(Default)]
struct RoundCursor {
    after: Option<IntentKey>,
    ceiling: Option<IntentKey>,
}

impl RoundCursor {
    fn candidates(&mut self, keys: &[IntentKey]) -> Vec<usize> {
        if keys.is_empty() {
            *self = Self::default();
            return Vec::new();
        }
        if self.ceiling.is_none() {
            self.ceiling = keys.iter().max().cloned();
        }
        let mut candidates = self.remaining(keys);
        if candidates.is_empty() {
            self.after = None;
            self.ceiling = keys.iter().max().cloned();
            candidates = self.remaining(keys);
        }
        candidates.sort_unstable_by(|&left, &right| keys[left].cmp(&keys[right]));
        candidates
    }

    fn remaining(&self, keys: &[IntentKey]) -> Vec<usize> {
        keys.iter()
            .enumerate()
            .filter(|(_, key)| {
                self.after.as_ref().is_none_or(|after| *key > after)
                    && self.ceiling.as_ref().is_some_and(|ceiling| *key <= ceiling)
            })
            .map(|(index, _)| index)
            .collect()
    }
}

#[derive(Debug, Default)]
struct ReplayReport {
    attempted: usize,
    /// Successful ACK operations or release pages, not necessarily full intents.
    applied: usize,
    completed: usize,
    abandoned: usize,
    deferred: usize,
    rows_released: usize,
    more: bool,
    interrupted: bool,
}

#[derive(Default)]
struct ReplayCursors {
    acknowledgements: RoundCursor,
    releases: releases::Cursor,
}

struct Snapshots {
    // None means scanning or waiting for this lane's next poll. It must not
    // reset the mutation cursor or authorize opening a database connection.
    acknowledgements: std::io::Result<Option<Vec<QueuedAckIntent>>>,
    releases: std::io::Result<Option<Vec<QueuedReleaseIntentView>>>,
}

impl Snapshots {
    /// An unselected lane is absent, not verified-empty: it must not reset any
    /// cursor or touch the other journal, pool, or archive on this thread.
    fn from_lane(
        kind: journal_scan::Kind,
        result: std::io::Result<Option<Vec<journal_scan::Intent>>>,
    ) -> Self {
        match kind {
            journal_scan::Kind::Ack => Self {
                acknowledgements: ack_snapshot(result),
                releases: Ok(None),
            },
            journal_scan::Kind::Release => Self {
                acknowledgements: Ok(None),
                releases: release_snapshot(result),
            },
        }
    }

    #[cfg(test)]
    fn read(config: &Config) -> Self {
        fn complete(
            config: &Config,
            kind: journal_scan::Kind,
        ) -> std::io::Result<Option<Vec<journal_scan::Intent>>> {
            let mut scanner = journal_scan::Scanner::new(kind);
            loop {
                if let Some(items) = scanner.poll(config, &AtomicBool::new(false))? {
                    return Ok(Some(items));
                }
            }
        }
        Self {
            acknowledgements: ack_snapshot(complete(config, journal_scan::Kind::Ack)),
            releases: release_snapshot(complete(config, journal_scan::Kind::Release)),
        }
    }

    fn needs_db(&self) -> bool {
        self.acknowledgements
            .as_ref()
            .is_ok_and(|items| items.as_ref().is_some_and(|items| !items.is_empty()))
            || self
                .releases
                .as_ref()
                .is_ok_and(|items| items.as_ref().is_some_and(|items| !items.is_empty()))
    }
}

fn ack_snapshot(
    result: std::io::Result<Option<Vec<journal_scan::Intent>>>,
) -> std::io::Result<Option<Vec<QueuedAckIntent>>> {
    result?
        .map(|items| {
            items
                .into_iter()
                .map(|item| match item {
                    journal_scan::Intent::Ack(intent) => Ok(intent),
                    journal_scan::Intent::Release(_) => {
                        Err(std::io::Error::other("release in acknowledgement snapshot"))
                    }
                })
                .collect()
        })
        .transpose()
}

fn release_snapshot(
    result: std::io::Result<Option<Vec<journal_scan::Intent>>>,
) -> std::io::Result<Option<Vec<QueuedReleaseIntentView>>> {
    result?
        .map(|items| {
            items
                .into_iter()
                .map(|item| match item {
                    journal_scan::Intent::Release(intent) => Ok(intent),
                    journal_scan::Intent::Ack(_) => {
                        Err(std::io::Error::other("acknowledgement in release snapshot"))
                    }
                })
                .collect()
        })
        .transpose()
}

struct ScanLane {
    scanner: journal_scan::Scanner,
    scanning: bool,
    next_scan: Option<Instant>,
}

impl ScanLane {
    fn new(kind: journal_scan::Kind) -> Self {
        Self {
            scanner: journal_scan::Scanner::new(kind),
            scanning: false,
            next_scan: None,
        }
    }

    fn poll(
        &mut self,
        config: &Config,
        shutdown: &AtomicBool,
        now: Instant,
    ) -> std::io::Result<Option<Vec<journal_scan::Intent>>> {
        if !shutdown.load(Ordering::Acquire)
            && !self.scanning
            && self.next_scan.is_some_and(|next| now < next)
        {
            return Ok(None);
        }
        let result = self.scanner.poll(config, shutdown);
        self.scanning = matches!(result.as_ref(), Ok(None));
        if !self.scanning {
            // Errors, missing journals, and completed scans all have their own
            // backoff. Another lane's catch-up cannot hammer this source.
            self.next_scan = Some(now + POLL_INTERVAL);
        }
        result
    }

    fn replay_finished(&mut self, report: &ReplayReport, now: Instant) {
        if (report.completed > 0 || report.applied > 0)
            && report.more
            && report.deferred == 0
            && !report.interrupted
        {
            self.next_scan = Some(now + CATCH_UP_INTERVAL);
        }
    }

    fn next_delay(&self, now: Instant) -> Duration {
        if self.scanning {
            SCAN_INTERVAL
        } else {
            self.next_scan
                .map_or(Duration::ZERO, |next| next.saturating_duration_since(now))
        }
    }
}

// Paired scan fixture for the existing cross-kind admission regressions. The
// live workers each poll a single ScanLane instead of serializing both sources.
#[cfg(test)]
struct SnapshotReaders {
    acknowledgements: ScanLane,
    releases: ScanLane,
}

#[cfg(test)]
impl SnapshotReaders {
    fn new() -> Self {
        Self {
            acknowledgements: ScanLane::new(journal_scan::Kind::Ack),
            releases: ScanLane::new(journal_scan::Kind::Release),
        }
    }

    fn poll(&mut self, config: &Config, shutdown: &AtomicBool, now: Instant) -> Snapshots {
        Snapshots {
            acknowledgements: ack_snapshot(self.acknowledgements.poll(config, shutdown, now)),
            releases: release_snapshot(self.releases.poll(config, shutdown, now)),
        }
    }

    fn next_delay(&self, now: Instant) -> Duration {
        self.acknowledgements
            .next_delay(now)
            .min(self.releases.next_delay(now))
    }
}

#[derive(Clone, Copy)]
enum Completion {
    Replayed,
    KeyConflict,
}

const fn replay_kind_name(kind: journal_scan::Kind) -> &'static str {
    match kind {
        journal_scan::Kind::Ack => "acknowledgement",
        journal_scan::Kind::Release => "release",
    }
}

/// One cancellation flag per incarnation. Restarting a failed lane must not
/// reactivate an older worker or reset the healthy lane's cancellation state.
struct ReplayWorker {
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ReplayWorker {
    fn spawn(config: &Config, kind: journal_scan::Kind) -> std::io::Result<Self> {
        let config = config.clone();
        Self::spawn_with(kind, move |shutdown| run(&config, kind, shutdown))
    }

    fn spawn_with(
        kind: journal_scan::Kind,
        work: impl FnOnce(&AtomicBool) + Send + 'static,
    ) -> std::io::Result<Self> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&shutdown);
        let handle = std::thread::Builder::new()
            .name(format!("degraded-{}-replay", replay_kind_name(kind)))
            .stack_size(mcp_agent_mail_core::worker_stack_size())
            .spawn(move || work(&stop))?;
        Ok(Self {
            shutdown,
            handle: Some(handle),
        })
    }

    fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    fn request_stop(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

impl Drop for ReplayWorker {
    fn drop(&mut self) {
        self.request_stop();
        if let Some(handle) = self.handle.take() {
            // Never detach a worker which may still own a database generation
            // or publish a receipt. Blocking DB/Git/log I/O has no hard deadline.
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
fn ensure_replay_workers(
    workers: &mut ReplayWorkerSlots,
    mut spawn: impl FnMut(journal_scan::Kind) -> std::io::Result<ReplayWorker>,
) -> Vec<(journal_scan::Kind, std::io::Error)> {
    let mut errors = Vec::new();
    for (slot, kind) in workers.iter_mut().zip(REPLAY_KINDS) {
        if slot.as_ref().is_some_and(ReplayWorker::is_finished) {
            drop(slot.take());
        }
        if slot.is_none() {
            match spawn(kind) {
                Ok(worker) => *slot = Some(worker),
                Err(error) => errors.push((kind, error)),
            }
        }
    }
    errors
}

fn stop_replay_workers(workers: &mut ReplayWorkerSlots) {
    // Signal BOTH lanes before joining either: a stalled lane must not keep
    // the other polling/mutating simply because its join happens second.
    for worker in workers.iter().flatten() {
        worker.request_stop();
    }
    for slot in workers {
        drop(slot.take());
    }
}

/// Freeze the mailbox selection for the pair, not just for each thread. A
/// partial-start retry must not place ACKs in one mailbox and releases in another.
struct ReplayWorkerGroup {
    config: Config,
    slots: ReplayWorkerSlots,
}

impl Drop for ReplayWorkerGroup {
    fn drop(&mut self) {
        stop_replay_workers(&mut self.slots);
    }
}

#[cfg(test)]
fn selected_replay_group<'a>(
    group: &'a mut Option<ReplayWorkerGroup>,
    config: &Config,
) -> &'a mut ReplayWorkerGroup {
    group.get_or_insert_with(|| ReplayWorkerGroup {
        config: config.clone(),
        slots: [None, None],
    })
}

pub(super) fn start(config: &Config) {
    // There is no persistent mailbox to recover for an in-memory database.
    if mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url).is_none()
    {
        return;
    }
    let mut workers = WORKERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let service = workers.get_or_insert_with(|| supervision::Service::new(config));
    let retargeted = service.config.database_url != config.database_url
        || service.config.storage_root != config.storage_root;
    let started = service.ensure_running();
    drop(workers);
    if retargeted {
        tracing::warn!(
            "durable replay retains its original mailbox until shutdown; restart required to retarget"
        );
    }
    if let Err(error) = started {
        tracing::warn!(%error, "could not start durable replay supervisor; startup retry required");
    }
}

pub(super) fn shutdown() {
    // Serialize the entire stop/start boundary. Neither worker takes this mutex.
    let mut workers = WORKERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    drop(workers.take());
}

fn sleep_until_next_pass(duration: Duration, shutdown: &AtomicBool) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if shutdown.load(Ordering::Acquire) {
            return true;
        }
        let step = remaining.min(Duration::from_millis(100));
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
    shutdown.load(Ordering::Acquire)
}

fn pool_config(config: &Config) -> DbPoolConfig {
    let mut selected = DbPoolConfig::from_env();
    selected.database_url.clone_from(&config.database_url);
    selected.storage_root = Some(config.storage_root.clone());
    selected.min_connections = 1;
    selected.max_connections = 1;
    selected.warmup_connections = 0;
    // Every serving path performs startup migrations before starting workers.
    selected.run_migrations = false;
    selected
}

fn run(config: &Config, kind: journal_scan::Kind, shutdown: &AtomicBool) {
    let mut cursors = ReplayCursors::default();
    let mut lane = ScanLane::new(kind);
    let mut pool = None;
    let kind_name = replay_kind_name(kind);
    // Give startup admission priority; no client request is needed thereafter.
    if sleep_until_next_pass(CATCH_UP_INTERVAL, shutdown) {
        return;
    }
    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        let snapshots = Snapshots::from_lane(kind, lane.poll(config, shutdown, Instant::now()));
        if !snapshots.needs_db() {
            // Do not retain a connection for an idle, healthy mailbox.
            pool = None;
        } else if pool.is_none() {
            match mcp_agent_mail_db::create_pool(&pool_config(config)) {
                Ok(created) => pool = Some(created),
                Err(error) => tracing::warn!(
                    kind = kind_name, %error, "durable intent replay awaits live database admission"
                ),
            }
        }
        let [ack, release] = fastmcp_core::block_on(async {
            match Cx::current() {
                Some(cx) => {
                    replay_snapshots(
                        &cx,
                        pool.as_ref(),
                        config,
                        &mut cursors,
                        shutdown,
                        snapshots,
                    )
                    .await
                }
                None => [
                    Err("durable replay has no runtime context".to_string()),
                    Err("durable replay has no runtime context".to_string()),
                ],
            }
        });
        let result = match kind {
            journal_scan::Kind::Ack => ack,
            journal_scan::Kind::Release => release,
        };
        let deferred = match result {
            Ok(report) => {
                if report.attempted > 0 || report.interrupted {
                    tracing::info!(target: "maintenance", event = "degraded_intent_replay",
                        kind = kind_name, attempted = report.attempted, applied = report.applied,
                        completed = report.completed, abandoned = report.abandoned,
                        rows_released = report.rows_released, deferred = report.deferred,
                        more = report.more, interrupted = report.interrupted,
                        "durable closeout replay pass completed");
                }
                lane.replay_finished(&report, Instant::now());
                report.deferred > 0 || report.interrupted
            }
            Err(error) => {
                tracing::warn!(kind = kind_name, %error,
                    "durable closeout replay deferred; journal preserved");
                true
            }
        };
        if deferred {
            // A repaired database may have replaced the old generation.
            pool = None;
        }
        let interval = lane.next_delay(Instant::now());
        if sleep_until_next_pass(interval, shutdown) {
            return;
        }
    }
}

/// Dispatch verified work. Production supplies only its worker's lane; paired
/// snapshots remain useful for admission tests, but are not concurrent here.
/// No public tool handler is called: it would recursively drain or requeue.
async fn replay_snapshots(
    cx: &Cx,
    pool: Option<&DbPool>,
    config: &Config,
    cursors: &mut ReplayCursors,
    shutdown: &AtomicBool,
    snapshots: Snapshots,
) -> [Result<ReplayReport, String>; 2] {
    let ack = match snapshots.acknowledgements {
        Ok(None) => Ok(ReplayReport::default()),
        Ok(Some(intents)) if intents.is_empty() => {
            cursors.acknowledgements = RoundCursor::default();
            Ok(ReplayReport::default())
        }
        Ok(Some(intents)) => match pool {
            Some(pool) => {
                replay_ack_batch(
                    cx,
                    pool,
                    config,
                    &mut cursors.acknowledgements,
                    shutdown,
                    &intents,
                )
                .await
            }
            None => Err("durable acknowledgement database unavailable".to_string()),
        },
        Err(error) => Err(format!(
            "durable acknowledgement journal unavailable: {error}"
        )),
    };
    let release = match snapshots.releases {
        Ok(None) => Ok(ReplayReport::default()),
        Ok(Some(intents)) if intents.is_empty() => {
            cursors.releases = releases::Cursor::default();
            Ok(ReplayReport::default())
        }
        Ok(Some(intents)) => match pool {
            Some(pool) => {
                releases::replay_batch(cx, pool, config, &mut cursors.releases, shutdown, &intents)
                    .await
            }
            None => Err("durable release database unavailable".to_string()),
        },
        Err(error) => Err(format!("durable release journal unavailable: {error}")),
    };
    [ack, release]
}

fn validate_pool_binding(pool: &DbPool, config: &Config) -> Result<(), String> {
    let selected =
        mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url)
            .ok_or_else(|| "durable replay requires a file-backed mailbox".to_string())?;
    let source = std::fs::canonicalize(pool.sqlite_path()).map_err(|e| e.to_string())?;
    let selected = std::fs::canonicalize(selected).map_err(|e| e.to_string())?;
    let source_root = std::fs::canonicalize(pool.storage_root()).map_err(|e| e.to_string())?;
    let selected_root = std::fs::canonicalize(&config.storage_root).map_err(|e| e.to_string())?;
    if source != selected || source_root != selected_root {
        return Err("durable replay pool is not bound to the configured mailbox".to_string());
    }
    Ok(())
}

async fn validate_live_pool(cx: &Cx, pool: &DbPool, config: &Config) -> Result<(), String> {
    validate_pool_binding(pool, config)?;
    // Even a correctly named archive snapshot must not authorize live mutation.
    let conn = db_value(match pool.acquire(cx).await {
        Outcome::Ok(conn) => Outcome::Ok(conn),
        Outcome::Err(error) => Outcome::Err(DbError::Sqlite(error.to_string())),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    })?;
    let rows = conn
        .query_sync("PRAGMA query_only", &[])
        .map_err(|e| e.to_string())?;
    let query_only = rows
        .first()
        .ok_or_else(|| "durable replay source did not report its mode".to_string())?
        .get_as::<i64>(0)
        .map_err(|e| e.to_string())?;
    drop(conn);
    if query_only != 0 {
        return Err("query-only snapshots cannot authorize durable replay".to_string());
    }
    Ok(())
}

async fn replay_ack_batch(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut RoundCursor,
    shutdown: &AtomicBool,
    intents: &[QueuedAckIntent],
) -> Result<ReplayReport, String> {
    let mut report = ReplayReport::default();
    if shutdown.load(Ordering::Acquire) {
        report.interrupted = true;
        return Ok(report);
    }
    // Background replay bypasses the public tool's WriteDbPool guard. Keep
    // identities, mutation and completion on one live database generation.
    let _write_activity = mcp_agent_mail_db::write_barrier::begin_write_activity();
    validate_live_pool(cx, pool, config).await?;
    let keys: Vec<_> = intents
        .iter()
        .map(|intent| (intent.created_ts, intent.content_sha256.clone()))
        .collect();
    let candidates = cursor.candidates(&keys);
    report.more = candidates.len() > MAX_ATTEMPTS;
    let ctx = McpContext::new(cx.clone(), 0);
    for index in candidates.into_iter().take(MAX_ATTEMPTS) {
        if shutdown.load(Ordering::Acquire) || ctx.checkpoint().is_err() {
            report.interrupted = true;
            report.more = true;
            break;
        }
        let intent = &intents[index];
        cursor.after = Some(keys[index].clone());
        report.attempted += 1;
        match apply_ack(&ctx, pool, intent).await {
            Ok(completion) => {
                if matches!(completion, Completion::Replayed) {
                    report.applied += 1;
                }
                match append_ack_completion(config, intent, completion) {
                    Ok(()) => {
                        report.completed += 1;
                        if matches!(completion, Completion::KeyConflict) {
                            report.abandoned += 1;
                        }
                    }
                    Err(error) => {
                        report.deferred += 1;
                        tracing::warn!(intent_id = %intent.intent_id, %error,
                            "acknowledgement outcome resolved but completion receipt unavailable; safe replay retained");
                    }
                }
            }
            Err(error) => {
                report.deferred += 1;
                tracing::warn!(intent_id = %intent.intent_id, %error,
                    "durable acknowledgement remains queued");
            }
        }
    }
    Ok(report)
}

async fn apply_ack(
    ctx: &McpContext,
    pool: &DbPool,
    intent: &QueuedAckIntent,
) -> Result<Completion, String> {
    // Recovery must never mint a project or agent from an obsolete intent.
    // Missing identities remain pending; a restore can bring them back later.
    let project = resolve_existing_project(ctx, pool, &intent.project_key)
        .await
        .map_err(|e| e.to_string())?;
    let project_id = project
        .id
        .filter(|id| *id > 0)
        .ok_or_else(|| "durable replay project has no positive identity".to_string())?;
    let agent = resolve_agent(
        ctx,
        pool,
        project_id,
        &intent.agent_name,
        &project.slug,
        &project.human_key,
    )
    .await
    .map_err(|e| e.to_string())?;
    let agent_id = agent
        .id
        .filter(|id| *id > 0)
        .ok_or_else(|| "durable replay agent has no positive identity".to_string())?;
    if let Some(idempotency) = &intent.idempotency {
        // Do not call the tool handler: it would rediscover ambient config,
        // recursively drain the queue, and append another intent on failure.
        let claim = IdempotencyClaim {
            project_id,
            tool: "acknowledge_message",
            key: &idempotency.key,
            fingerprint: &idempotency.fingerprint,
        };
        let outcome = db_value(
            queries::acknowledge_message_idempotent(
                ctx.cx(),
                pool,
                agent_id,
                intent.message_id,
                claim,
            )
            .await,
        )?;
        Ok(match outcome {
            IdempotentOutcome::Fresh(_) | IdempotentOutcome::Replayed(_) => Completion::Replayed,
            IdempotentOutcome::Conflict(_) => Completion::KeyConflict,
        })
    } else {
        db_value(queries::acknowledge_message(ctx.cx(), pool, agent_id, intent.message_id).await)?;
        Ok(Completion::Replayed)
    }
}

fn db_value<T>(outcome: Outcome<T, DbError>) -> Result<T, String> {
    match outcome {
        Outcome::Ok(value) => Ok(value),
        Outcome::Err(error) => {
            mcp_agent_mail_db::corruption_circuit_breaker().observe_error(&error);
            Err(error.to_string())
        }
        Outcome::Cancelled(_) => Err("durable closeout replay cancelled".to_string()),
        Outcome::Panicked(_) => Err("durable closeout replay panicked".to_string()),
    }
}

fn append_ack_completion(
    config: &Config,
    intent: &QueuedAckIntent,
    completion: Completion,
) -> std::io::Result<()> {
    let (status, detail) = match completion {
        Completion::Replayed => (journal::REPLAY_STATUS_REPLAYED, None),
        Completion::KeyConflict => (
            journal::REPLAY_STATUS_ABANDONED,
            Some("IDEMPOTENCY_KEY_CONFLICT: the original acknowledgement stands"),
        ),
    };
    let mut record = json!({
        "schema_version": journal::ACK_INTENT_SCHEMA_VERSION,
        "kind": journal::ACK_INTENT_REPLAY_KIND,
        "intent_id": intent.intent_id,
        "intent_content_sha256": intent.content_sha256,
        "replayed_ts": mcp_agent_mail_db::now_micros(),
        "status": status,
        "error_detail": detail,
    });
    record["content_sha256"] = json!(journal::hash_json_value(&record));
    // Unlike the opportunistic helper, propagate receipt publication failure
    // so this pass cannot report the intent durably completed when it was not.
    journal::append_jsonl(
        config,
        journal::ACK_INTENT_LOG_FILE,
        journal::ACK_INTENT_LOCK_FILE,
        &record,
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;

    #[test]
    fn lane_snapshot_preserves_absence_and_confines_source_errors() {
        for kind in REPLAY_KINDS {
            let absent = Snapshots::from_lane(kind, Ok(None));
            assert!(absent.acknowledgements.unwrap().is_none());
            assert!(absent.releases.unwrap().is_none());
            let failed = Snapshots::from_lane(kind, Err(std::io::Error::other("unreadable")));
            assert!(!failed.needs_db());
            let empty = Snapshots::from_lane(kind, Ok(Some(Vec::new())));
            assert!(!empty.needs_db());
            match kind {
                journal_scan::Kind::Ack => {
                    assert!(failed.acknowledgements.is_err());
                    assert!(failed.releases.unwrap().is_none());
                    assert_eq!(
                        empty.acknowledgements.unwrap().unwrap(),
                        [] as [QueuedAckIntent; 0]
                    );
                    assert!(empty.releases.unwrap().is_none());
                }
                journal_scan::Kind::Release => {
                    assert!(failed.acknowledgements.unwrap().is_none());
                    assert!(failed.releases.is_err());
                    assert!(empty.acknowledgements.unwrap().is_none());
                    assert_eq!(
                        empty.releases.unwrap().unwrap(),
                        [] as [QueuedReleaseIntentView; 0]
                    );
                }
            }
        }
    }

    fn sleeping_replay_worker(kind: journal_scan::Kind) -> std::io::Result<ReplayWorker> {
        ReplayWorker::spawn_with(kind, |stop| {
            assert!(sleep_until_next_pass(Duration::from_secs(3600), stop));
        })
    }

    #[test]
    fn partial_start_keeps_the_original_mailbox_until_the_group_is_stopped() {
        let first = Config {
            database_url: "sqlite:///first.sqlite3".to_string(),
            storage_root: std::path::PathBuf::from("first-archive"),
            ..Config::default()
        };
        let second = Config {
            database_url: "sqlite:///second.sqlite3".to_string(),
            storage_root: std::path::PathBuf::from("second-archive"),
            ..first.clone()
        };
        let mut group = None;
        selected_replay_group(&mut group, &first).slots[1] =
            Some(sleeping_replay_worker(journal_scan::Kind::Release).unwrap());
        let selected = selected_replay_group(&mut group, &second);
        assert!(selected.slots[0].is_none());
        assert_eq!(selected.config.database_url, first.database_url);
        assert_eq!(selected.config.storage_root, first.storage_root);
        drop(group.take());
        let restarted = selected_replay_group(&mut group, &second);
        assert_eq!(restarted.config.database_url, second.database_url);
        assert_eq!(restarted.config.storage_root, second.storage_root);
    }

    #[test]
    fn failed_lane_start_does_not_stop_the_peer_and_retries_only_the_missing_lane() {
        let mut workers: ReplayWorkerSlots = [None, None];
        let errors = ensure_replay_workers(&mut workers, |kind| match kind {
            journal_scan::Kind::Ack => Err(std::io::Error::other("injected spawn refusal")),
            journal_scan::Kind::Release => sleeping_replay_worker(kind),
        });
        assert_eq!(errors.len(), 1);
        assert!(workers[0].is_none());
        let peer = workers[1]
            .as_ref()
            .unwrap()
            .handle
            .as_ref()
            .unwrap()
            .thread()
            .id();
        let mut attempts = 0;
        let errors = ensure_replay_workers(&mut workers, |kind| {
            assert!(matches!(kind, journal_scan::Kind::Ack));
            attempts += 1;
            sleeping_replay_worker(kind)
        });
        assert!(errors.is_empty());
        assert_eq!(attempts, 1);
        assert_eq!(
            workers[1]
                .as_ref()
                .unwrap()
                .handle
                .as_ref()
                .unwrap()
                .thread()
                .id(),
            peer
        );
        let stopped = workers
            .each_ref()
            .map(|slot| Arc::clone(&slot.as_ref().unwrap().shutdown));
        stop_replay_workers(&mut workers);
        assert!(workers.iter().all(Option::is_none));
        assert!(stopped.iter().all(|stop| stop.load(Ordering::Acquire)));
    }

    #[test]
    fn finished_lane_restart_preserves_the_peer_and_never_reuses_cancellation() {
        let first = ReplayWorker::spawn_with(journal_scan::Kind::Ack, |_| {}).unwrap();
        let old_stop = Arc::clone(&first.shutdown);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !first.is_finished() {
            assert!(Instant::now() < deadline, "first worker did not exit");
            std::thread::yield_now();
        }
        let mut workers = [
            Some(first),
            Some(sleeping_replay_worker(journal_scan::Kind::Release).unwrap()),
        ];
        let peer_stop = Arc::clone(&workers[1].as_ref().unwrap().shutdown);
        let errors = ensure_replay_workers(&mut workers, sleeping_replay_worker);
        assert!(errors.is_empty());
        let new_stop = Arc::clone(&workers[0].as_ref().unwrap().shutdown);
        assert!(
            old_stop.load(Ordering::Acquire),
            "old incarnation was joined"
        );
        assert!(!Arc::ptr_eq(&old_stop, &new_stop));
        assert!(Arc::ptr_eq(
            &peer_stop,
            &workers[1].as_ref().unwrap().shutdown
        ));
        assert!(!new_stop.load(Ordering::Acquire));
        assert!(!peer_stop.load(Ordering::Acquire));
        stop_replay_workers(&mut workers);
    }

    #[test]
    fn shutdown_signals_both_lanes_before_joining_either() {
        let (peer_stopped_tx, peer_stopped_rx) = std::sync::mpsc::channel();
        let (observed_tx, observed_rx) = std::sync::mpsc::channel();
        let ack = ReplayWorker::spawn_with(journal_scan::Kind::Ack, move |stop| {
            let cancelled = sleep_until_next_pass(Duration::from_secs(3600), stop);
            let peer_stopped = peer_stopped_rx.recv_timeout(Duration::from_secs(5)).is_ok();
            observed_tx.send(cancelled && peer_stopped).unwrap();
        })
        .unwrap();
        let release = ReplayWorker::spawn_with(journal_scan::Kind::Release, move |stop| {
            if sleep_until_next_pass(Duration::from_secs(3600), stop) {
                let _ = peer_stopped_tx.send(());
            }
        })
        .unwrap();
        let mut workers = [Some(ack), Some(release)];
        stop_replay_workers(&mut workers);
        assert!(observed_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        assert!(workers.iter().all(Option::is_none));
    }

    fn isolate_replay_worker_test() -> bool {
        const CHILD: &str = "AM_TEST_REPLAY_LANES_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named libtest thread");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .output()
            .expect("run isolated replay worker test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "isolated {name} failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    fn wait_for_replay_observation(mut observed: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if observed() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn closeout_workers_complete_while_archive_repair_defers_on_the_real_project_lock() {
        if isolate_replay_worker_test() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let config = Config {
                storage_root: temp.path().join("archive"),
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                    &temp.path().join("mail.sqlite3"),
                ),
                ..Config::default()
            };
            std::fs::create_dir_all(&config.storage_root).unwrap();
            let project_root = temp.path().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let mut selected = pool_config(&config);
            selected.run_migrations = true;
            let pool = DbPool::new(&selected).unwrap();
            let cx = Cx::for_testing();
            let conn = fastmcp_core::block_on(pool.acquire(&cx))
                .into_result()
                .unwrap();
            let key = project_root.to_string_lossy().replace('\'', "''");
            conn.execute_raw(&format!("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'replay', '{key}', 1)")).unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1)").unwrap();
            drop(conn);
            let message = fastmcp_core::block_on(queries::create_message_with_recipients(
                &cx,
                &pool,
                71,
                81,
                "ack during archive contention",
                "body",
                None,
                "normal",
                true,
                "[]",
                &[(81, "to")],
            ))
            .into_result()
            .unwrap();
            let message_id = message.id.unwrap();
            let lease = fastmcp_core::block_on(queries::create_file_reservations(
                &cx,
                &pool,
                71,
                81,
                &["src/owned.rs"],
                3600,
                true,
                "queued release",
            ))
            .into_result()
            .unwrap();
            let lease_id = lease[0].id.unwrap();
            let archive = mcp_agent_mail_storage::ensure_archive(&config, "replay").unwrap();
            mcp_agent_mail_storage::flush_async_commits();
            let generation = fastmcp_core::block_on(queries::db_generation_id(&cx, &pool))
                .into_result()
                .unwrap()
                .expect("live generation");
            let stable = archive.root.join("file_reservations").join(
                mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
                    Some(&generation),
                    lease_id,
                ),
            );
            let mut record = json!({
                "schema_version": 1, "kind": journal::RELEASE_INTENT_KIND,
                "created_ts": mcp_agent_mail_db::now_micros(),
                "project_key": "replay", "agent_name": "BlueLake",
                "paths": null, "file_reservation_ids": [lease_id],
                "failure": {"stage": "test", "error_detail": "outage"},
            });
            let hash = journal::hash_json_value(&record);
            record["intent_id"] = json!(&hash[..16]);
            record["content_sha256"] = json!(hash);
            journal::append_jsonl(
                &config,
                journal::RELEASE_INTENT_LOG_FILE,
                ".release_file_reservations.jsonl.lock",
                &record,
            )
            .unwrap();

            // Declare the replay slots first so unwinding drops/unlocks the
            // lock owner before joining a replay which may be waiting on it.
            let mut workers: ReplayWorkerSlots = [None, None];
            let (locked_tx, locked_rx) = std::sync::mpsc::channel();
            let (unlocked_tx, unlocked_rx) = std::sync::mpsc::channel();
            let locker = ReplayWorker::spawn_with(journal_scan::Kind::Release, move |stop| {
                let result = mcp_agent_mail_storage::with_project_lock(&archive, || {
                    locked_tx.send(()).unwrap();
                    // Cleanup deadline only: the assertions below require the
                    // lock to be explicitly released after ACK recovery.
                    if !sleep_until_next_pass(Duration::from_secs(45), stop) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "archive blocker expired",
                        )
                        .into());
                    }
                    Ok(())
                });
                let _ = unlocked_tx.send(result);
            })
            .unwrap();
            locked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            workers[1] = Some(ReplayWorker::spawn(&config, journal_scan::Kind::Release).unwrap());
            let release_applied = wait_for_replay_observation(|| {
                fastmcp_core::block_on(queries::get_reservations_by_ids(&cx, &pool, &[lease_id]))
                    .into_result()
                    .is_ok_and(|rows| rows.first().is_some_and(|row| row.released_ts.is_some()))
            });
            // Queue this ACK only AFTER the release reached its database
            // mutation. It cannot pass merely by winning the first poll race.
            journal::append_ack_intent(
                &config, "replay", "BlueLake", message_id, "test", "outage", None,
            )
            .unwrap();
            let spawn_errors =
                ensure_replay_workers(&mut workers, |kind| ReplayWorker::spawn(&config, kind));
            let ack_completed = wait_for_replay_observation(|| {
                journal::read_queued_ack_intents(&config).is_ok_and(|intents| intents.is_empty())
            });
            // Project-lock waiting is now bounded: database closeout and its
            // receipt must finish even though the archive remains unavailable.
            let release_completed_while_locked = wait_for_replay_observation(|| {
                journal::read_queued_release_intents(&config)
                    .is_ok_and(|intents| intents.is_empty())
            });
            let archive_deferred_while_locked = !stable.exists();
            let release_still_running = !workers[1].as_ref().unwrap().is_finished();
            // An ACK completion marker alone is insufficient: verify the real
            // recipient receipt as well, while the archive lock is still held.
            let conn = fastmcp_core::block_on(pool.acquire(&cx))
                .into_result()
                .unwrap();
            let rows = conn
                .query_sync(
                    "SELECT ack_ts FROM message_recipients WHERE message_id = ? AND agent_id = 81",
                    &[message_id.into()],
                )
                .unwrap();
            let ack_ts = rows[0].get_named::<Option<i64>>("ack_ts").unwrap();
            drop(conn);
            drop(locker);
            let explicitly_unlocked = unlocked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let release_completed = wait_for_replay_observation(|| {
                journal::read_queued_release_intents(&config)
                    .is_ok_and(|intents| intents.is_empty())
            });
            stop_replay_workers(&mut workers);
            assert!(
                explicitly_unlocked.is_ok(),
                "archive lock expired before explicit release"
            );
            assert!(spawn_errors.is_empty());
            assert!(
                release_applied,
                "release never reached its database mutation"
            );
            assert!(release_completed_while_locked && release_still_running);
            assert!(
                archive_deferred_while_locked,
                "repair wrote through a held project lock"
            );
            assert!(ack_completed && ack_ts.is_some_and(|ts| ts > 0));
            assert!(
                release_completed,
                "release did not finish after archive admission reopened"
            );
            assert!(workers.iter().all(Option::is_none));
            // Completion only certified the database. The existing ledger
            // must still drive authoritative archive repair after unlocking.
            let repaired = mcp_agent_mail_storage::recovery::reservation_reconcile::reconcile_reservation_releases(
                &cx, &pool, &config,
                &mut mcp_agent_mail_storage::recovery::reservation_reconcile::ReservationReconcileCursor::default(),
                &AtomicBool::new(false),
            ).unwrap();
            assert!(repaired.repaired > 0);
            let archived: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&stable).unwrap()).unwrap();
            assert_eq!(archived["id"], lease_id);
            assert_eq!(archived["db_generation"], generation);
            assert!(archived["released_ts"].is_string());
            mcp_agent_mail_storage::flush_async_commits();
        });
    }

    fn append_release_fixture(config: &Config, lease_id: i64) {
        let mut record = json!({
            "schema_version": 1, "kind": journal::RELEASE_INTENT_KIND,
            "created_ts": mcp_agent_mail_db::now_micros(),
            "project_key": "/replay", "agent_name": "BlueLake",
            "paths": null, "file_reservation_ids": [lease_id],
            "failure": {"stage": "test", "error_detail": "busy"},
        });
        let hash = journal::hash_json_value(&record);
        record["intent_id"] = json!(&hash[..16]);
        record["content_sha256"] = json!(hash);
        journal::append_jsonl(
            config,
            journal::RELEASE_INTENT_LOG_FILE,
            ".release_file_reservations.jsonl.lock",
            &record,
        )
        .unwrap();
    }

    #[test]
    fn partial_scans_do_not_reset_cursors_or_admit_database_work() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        journal::append_ack_intent(&config, "/replay", "BlueLake", 1, "test", "busy", None)
            .unwrap();
        let path = journal::log_path(&config, journal::ACK_INTENT_LOG_FILE);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&vec![b'\n'; 256])
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let stop = AtomicBool::new(false);
        let now = Instant::now();
        let mut readers = SnapshotReaders::new();
        let first = readers.poll(&config, &stop, now);
        assert!(first.acknowledgements.as_ref().unwrap().is_none());
        assert!(
            !first.needs_db(),
            "a valid prefix is not an admitted snapshot"
        );
        assert_eq!(readers.next_delay(now), SCAN_INTERVAL);
        let mut cursors = ReplayCursors::default();
        let after = (100, "a".repeat(64));
        cursors.acknowledgements.after = Some(after.clone());
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let cx = Cx::current().unwrap();
            let results = replay_snapshots(&cx, None, &config, &mut cursors, &stop, first).await;
            assert!(
                results
                    .into_iter()
                    .all(|result| result.unwrap().attempted == 0)
            );
        });
        assert_eq!(cursors.acknowledgements.after, Some(after));
        let second = readers.poll(&config, &stop, now + SCAN_INTERVAL);
        assert!(second.needs_db());
        assert_eq!(second.acknowledgements.unwrap().unwrap().len(), 1);
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn source_error_backoff_does_not_throttle_the_other_lanes_catch_up() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        journal::append_jsonl(
            &config,
            journal::ACK_INTENT_LOG_FILE,
            journal::ACK_INTENT_LOCK_FILE,
            &json!({"schema_version": 99, "kind": journal::ACK_INTENT_KIND}),
        )
        .unwrap();
        append_release_fixture(&config, 1);
        let now = Instant::now();
        let stop = AtomicBool::new(false);
        let mut readers = SnapshotReaders::new();
        let first = readers.poll(&config, &stop, now);
        assert!(first.acknowledgements.is_err());
        assert_eq!(first.releases.unwrap().unwrap().len(), 1);
        readers.releases.replay_finished(
            &ReplayReport {
                completed: MAX_ATTEMPTS,
                more: true,
                ..ReplayReport::default()
            },
            now,
        );
        assert_eq!(readers.next_delay(now), CATCH_UP_INTERVAL);
        let fast = readers.poll(&config, &stop, now + CATCH_UP_INTERVAL);
        assert!(
            fast.acknowledgements.unwrap().is_none(),
            "error lane keeps its own backoff"
        );
        assert_eq!(fast.releases.unwrap().unwrap().len(), 1);
        assert!(
            readers
                .poll(&config, &stop, now + POLL_INTERVAL)
                .acknowledgements
                .is_err()
        );
    }

    #[test]
    fn late_unknown_ack_schema_cannot_mutate_db_while_release_lane_recovers() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            database_url: format!("sqlite://{}", temp.path().join("mail.sqlite3").display()),
            ..Config::default()
        };
        let mut selected = pool_config(&config);
        selected.run_migrations = true;
        let pool = DbPool::new(&selected).unwrap();
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let cx = Cx::current().unwrap();
            let project = queries::ensure_project(&cx, &pool, "/replay")
                .await
                .into_result()
                .unwrap();
            let project_id = project.id.unwrap();
            let agent = queries::register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "test",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .unwrap();
            let agent_id = agent.id.unwrap();
            let message = queries::create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                agent_id,
                "must stay unacknowledged",
                "body",
                None,
                "normal",
                true,
                "[]",
                &[(agent_id, "to")],
            )
            .await
            .into_result()
            .unwrap();
            let leases = queries::create_file_reservations(
                &cx,
                &pool,
                project_id,
                agent_id,
                &["src/recover.rs"],
                3600,
                true,
                "pending release",
            )
            .await
            .into_result()
            .unwrap();
            let lease_id = leases[0].id.unwrap();
            append_release_fixture(&config, lease_id);
            journal::append_ack_intent(
                &config,
                "/replay",
                "BlueLake",
                message.id.unwrap(),
                "test",
                "busy",
                None,
            )
            .unwrap();
            let path = journal::log_path(&config, journal::ACK_INTENT_LOG_FILE);
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(&vec![b'\n'; 256])
                .unwrap();
            journal::append_jsonl(
                &config,
                journal::ACK_INTENT_LOG_FILE,
                journal::ACK_INTENT_LOCK_FILE,
                &json!({"schema_version": 99, "kind": journal::ACK_INTENT_REPLAY_KIND}),
            )
            .unwrap();
            let before = std::fs::read(&path).unwrap();
            let stop = AtomicBool::new(false);
            let now = Instant::now();
            let mut readers = SnapshotReaders::new();
            let mut cursors = ReplayCursors::default();
            let first = readers.poll(&config, &stop, now);
            assert!(first.acknowledgements.as_ref().unwrap().is_none());
            assert!(first.needs_db(), "the independent release is ready");
            let [ack, release] =
                replay_snapshots(&cx, Some(&pool), &config, &mut cursors, &stop, first).await;
            assert_eq!(ack.unwrap().attempted, 0);
            assert_eq!(release.unwrap().completed, 1);
            let next = readers.poll(&config, &stop, now + SCAN_INTERVAL);
            assert!(next.acknowledgements.is_err());
            let [ack, release] =
                replay_snapshots(&cx, Some(&pool), &config, &mut cursors, &stop, next).await;
            assert!(ack.is_err());
            assert_eq!(release.unwrap().attempted, 0);
            let conn = pool.acquire(&cx).await.into_result().unwrap();
            let rows = conn
                .query_sync(
                    "SELECT ack_ts FROM message_recipients WHERE message_id = ?",
                    &[message.id.unwrap().into()],
                )
                .unwrap();
            assert_eq!(rows[0].get_named::<Option<i64>>("ack_ts").unwrap(), None);
            drop(conn);
            let rows = queries::get_reservations_by_ids(&cx, &pool, &[lease_id])
                .await
                .into_result()
                .unwrap();
            assert!(rows[0].released_ts.is_some_and(|ts| ts > 0));
            assert_eq!(
                journal::read_queued_release_intents(&config).unwrap(),
                [] as [QueuedReleaseIntentView; 0]
            );
            assert_eq!(std::fs::read(path).unwrap(), before);
            mcp_agent_mail_storage::flush_async_commits();
        });
    }

    fn keys(values: &[i64]) -> Vec<IntentKey> {
        values
            .iter()
            .map(|value| (*value, format!("{value:064}")))
            .collect()
    }

    #[test]
    fn finite_round_revisits_failures_despite_new_arrivals() {
        let mut cursor = RoundCursor::default();
        let original = keys(&[1, 2, 3]);
        assert_eq!(cursor.candidates(&original), vec![0, 1, 2]);
        cursor.after = Some(original[1].clone());
        let growing = keys(&[1, 2, 3, 4, 5]);
        assert_eq!(cursor.candidates(&growing), vec![2]);
        cursor.after = Some(growing[2].clone());
        assert_eq!(cursor.candidates(&growing), vec![0, 1, 2, 3, 4]);
        assert_eq!(cursor.ceiling, Some(growing[4].clone()));
    }

    #[test]
    fn round_handles_compaction_empty_snapshots_and_equal_timestamps() {
        let mut cursor = RoundCursor::default();
        let original = vec![(1, "b".into()), (1, "a".into())];
        assert_eq!(cursor.candidates(&original), vec![1, 0]);
        cursor.after = Some(original[1].clone());
        assert_eq!(cursor.candidates(&original), vec![0]);
        let compacted = keys(&[2]);
        assert_eq!(cursor.candidates(&compacted), vec![0]);
        assert_eq!(cursor.candidates(&[]), [] as [usize; 0]);
        assert!(cursor.after.is_none());
        assert!(cursor.ceiling.is_none());
    }

    #[test]
    fn completion_receipts_use_existing_reader_contract_and_do_not_expose_keys() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        let claim = journal::AckIntentIdempotency {
            key: "private-key".into(),
            fingerprint: "a".repeat(64),
        };
        journal::append_ack_intent(
            &config,
            "/replay",
            "BlueLake",
            42,
            "test",
            "busy",
            Some(&claim),
        )
        .unwrap();
        let intents = journal::read_queued_ack_intents(&config).unwrap();
        append_ack_completion(&config, &intents[0], Completion::Replayed).unwrap();
        assert_eq!(
            journal::read_queued_ack_intents(&config).unwrap(),
            [] as [QueuedAckIntent; 0]
        );
        let text =
            std::fs::read_to_string(journal::log_path(&config, journal::ACK_INTENT_LOG_FILE))
                .unwrap();
        let receipt: serde_json::Value =
            serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert!(!receipt.to_string().contains("private-key"));
        assert_eq!(receipt["intent_content_sha256"], intents[0].content_sha256);
    }

    #[test]
    fn queued_ack_lands_without_another_client_call_and_preserves_first_receipts() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            database_url: format!("sqlite://{}", temp.path().join("mail.sqlite3").display()),
            ..Config::default()
        };
        let mut selected = pool_config(&config);
        selected.run_migrations = true;
        let pool = DbPool::new(&selected).unwrap();
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let cx = Cx::current().unwrap();
            let project = queries::ensure_project(&cx, &pool, "/replay")
                .await
                .into_result()
                .unwrap();
            let agent = queries::register_agent(
                &cx,
                &pool,
                project.id.unwrap(),
                "BlueLake",
                "codex-cli",
                "test",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .unwrap();
            let message = queries::create_message_with_recipients(
                &cx,
                &pool,
                project.id.unwrap(),
                agent.id.unwrap(),
                "queued ack",
                "body",
                None,
                "normal",
                true,
                "[]",
                &[(agent.id.unwrap(), "to")],
            )
            .await
            .into_result()
            .unwrap();
            let claim = journal::AckIntentIdempotency {
                key: "original-key".into(),
                fingerprint: mcp_agent_mail_tools::idempotency::compute_fingerprint(
                    "acknowledge_message",
                    &[
                        ("agent", "BlueLake".into()),
                        ("message_id", message.id.unwrap().to_string()),
                    ],
                ),
            };
            journal::append_ack_intent(
                &config,
                "/replay",
                "BlueLake",
                message.id.unwrap(),
                "test",
                "database unavailable",
                Some(&claim),
            )
            .unwrap();
            let intents = journal::read_queued_ack_intents(&config).unwrap();
            let stop = AtomicBool::new(false);
            let report = replay_ack_batch(
                &cx,
                &pool,
                &config,
                &mut RoundCursor::default(),
                &stop,
                &intents,
            )
            .await
            .unwrap();
            assert_eq!(
                (
                    report.attempted,
                    report.applied,
                    report.completed,
                    report.deferred
                ),
                (1, 1, 1, 0)
            );
            assert_eq!(
                journal::read_queued_ack_intents(&config).unwrap(),
                [] as [QueuedAckIntent; 0]
            );
            let conn = pool.acquire(&cx).await.into_result().unwrap();
            let first = conn
                .query_sync("SELECT read_ts, ack_ts FROM message_recipients", &[])
                .unwrap();
            let read_ts = first[0].get_named::<Option<i64>>("read_ts").unwrap();
            let ack_ts = first[0].get_named::<Option<i64>>("ack_ts").unwrap();
            assert!(read_ts.is_some() && ack_ts.is_some());
            drop(conn);
            // Model a concurrent worker holding a pre-completion snapshot.
            let repeated = replay_ack_batch(
                &cx,
                &pool,
                &config,
                &mut RoundCursor::default(),
                &stop,
                &intents,
            )
            .await
            .unwrap();
            assert_eq!(repeated.completed, 1);
            let conn = pool.acquire(&cx).await.into_result().unwrap();
            let repeated = conn
                .query_sync("SELECT read_ts, ack_ts FROM message_recipients", &[])
                .unwrap();
            assert_eq!(
                repeated[0].get_named::<Option<i64>>("read_ts").unwrap(),
                read_ts
            );
            assert_eq!(
                repeated[0].get_named::<Option<i64>>("ack_ts").unwrap(),
                ack_ts
            );
            drop(conn);

            // The original claim must not be bypassed when another queued
            // payload attempts to reuse its key after recovery.
            let second = queries::create_message_with_recipients(
                &cx,
                &pool,
                project.id.unwrap(),
                agent.id.unwrap(),
                "second",
                "body",
                None,
                "normal",
                true,
                "[]",
                &[(agent.id.unwrap(), "to")],
            )
            .await
            .into_result()
            .unwrap();
            let conflicting = journal::AckIntentIdempotency {
                key: claim.key.clone(),
                fingerprint: mcp_agent_mail_tools::idempotency::compute_fingerprint(
                    "acknowledge_message",
                    &[
                        ("agent", "BlueLake".into()),
                        ("message_id", second.id.unwrap().to_string()),
                    ],
                ),
            };
            journal::append_ack_intent(
                &config,
                "/replay",
                "BlueLake",
                second.id.unwrap(),
                "test",
                "database unavailable",
                Some(&conflicting),
            )
            .unwrap();
            let queued = journal::read_queued_ack_intents(&config).unwrap();
            let conflict = replay_ack_batch(
                &cx,
                &pool,
                &config,
                &mut RoundCursor::default(),
                &stop,
                &queued,
            )
            .await
            .unwrap();
            assert_eq!(
                (conflict.applied, conflict.completed, conflict.abandoned),
                (0, 1, 1)
            );
            assert_eq!(
                journal::read_queued_ack_intents(&config).unwrap(),
                [] as [QueuedAckIntent; 0]
            );
            let conn = pool.acquire(&cx).await.into_result().unwrap();
            let rows = conn
                .query_sync(
                    "SELECT ack_ts FROM message_recipients WHERE message_id = ?",
                    &[second.id.unwrap().into()],
                )
                .unwrap();
            assert_eq!(rows[0].get_named::<Option<i64>>("ack_ts").unwrap(), None);
        });
    }

    #[test]
    fn damaged_journal_does_not_strand_the_other_kind_of_closeout() {
        for broken_release in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let config = Config {
                storage_root: temp.path().to_path_buf(),
                database_url: format!("sqlite://{}", temp.path().join("mail.sqlite3").display()),
                retention_report_enabled: false,
                quota_enabled: false,
                messages_retention_days: 0,
                ..Config::default()
            };
            let mut selected = pool_config(&config);
            selected.run_migrations = true;
            let pool = DbPool::new(&selected).unwrap();
            let rt = RuntimeBuilder::current_thread().build().unwrap();
            rt.block_on(async {
                let cx = Cx::current().unwrap();
                let project = queries::ensure_project(&cx, &pool, "/replay")
                    .await
                    .into_result()
                    .unwrap();
                let project_id = project.id.unwrap();
                let agent = queries::register_agent(
                    &cx,
                    &pool,
                    project_id,
                    "BlueLake",
                    "codex-cli",
                    "test",
                    None,
                    None,
                    None,
                )
                .await
                .into_result()
                .unwrap();
                let agent_id = agent.id.unwrap();
                let message = queries::create_message_with_recipients(
                    &cx,
                    &pool,
                    project_id,
                    agent_id,
                    "queued",
                    "body",
                    None,
                    "normal",
                    true,
                    "[]",
                    &[(agent_id, "to")],
                )
                .await
                .into_result()
                .unwrap();
                let lease = queries::create_file_reservations(
                    &cx,
                    &pool,
                    project_id,
                    agent_id,
                    &["src/owned.rs"],
                    3600,
                    true,
                    "queued",
                )
                .await
                .into_result()
                .unwrap();
                let lease_id = lease[0].id.unwrap();
                let cutoff = mcp_agent_mail_db::now_micros();
                let release_lock = ".release_file_reservations.jsonl.lock";
                if broken_release {
                    journal::append_ack_intent(
                        &config,
                        "/replay",
                        "BlueLake",
                        message.id.unwrap(),
                        "test",
                        "busy",
                        None,
                    )
                    .unwrap();
                    journal::append_jsonl(
                        &config,
                        journal::RELEASE_INTENT_LOG_FILE,
                        release_lock,
                        &json!({"schema_version": 2, "kind": journal::RELEASE_INTENT_KIND}),
                    )
                    .unwrap();
                } else {
                    let mut record = json!({
                        "schema_version": 1, "kind": journal::RELEASE_INTENT_KIND,
                        "created_ts": cutoff, "project_key": "/replay", "agent_name": "BlueLake",
                        "paths": null, "file_reservation_ids": [lease_id],
                        "failure": {"stage": "test", "error_detail": "busy"},
                    });
                    let hash = journal::hash_json_value(&record);
                    record["intent_id"] = json!(&hash[..16]);
                    record["content_sha256"] = json!(hash);
                    journal::append_jsonl(
                        &config,
                        journal::RELEASE_INTENT_LOG_FILE,
                        release_lock,
                        &record,
                    )
                    .unwrap();
                    // A non-file journal is a real read error, not an empty queue.
                    std::fs::create_dir(journal::log_path(&config, journal::ACK_INTENT_LOG_FILE))
                        .unwrap();
                }
                let snapshots = Snapshots::read(&config);
                assert!(snapshots.needs_db());
                assert_eq!(snapshots.acknowledgements.is_err(), !broken_release);
                assert_eq!(snapshots.releases.is_err(), broken_release);
                let [ack, release] = replay_snapshots(
                    &cx,
                    Some(&pool),
                    &config,
                    &mut ReplayCursors::default(),
                    &AtomicBool::new(false),
                    snapshots,
                )
                .await;
                let conn = pool.acquire(&cx).await.into_result().unwrap();
                let rows = conn
                    .query_sync(
                        "SELECT ack_ts FROM message_recipients WHERE message_id = ?",
                        &[message.id.unwrap().into()],
                    )
                    .unwrap();
                assert_eq!(
                    rows[0]
                        .get_named::<Option<i64>>("ack_ts")
                        .unwrap()
                        .is_some(),
                    broken_release
                );
                drop(conn);
                let rows = queries::get_reservations_by_ids(&cx, &pool, &[lease_id])
                    .await
                    .into_result()
                    .unwrap();
                assert_eq!(
                    rows[0].released_ts.is_some_and(|ts| ts > 0),
                    !broken_release
                );
                if broken_release {
                    assert_eq!(ack.unwrap().completed, 1);
                    assert!(release.is_err());
                    assert_eq!(
                        journal::read_queued_ack_intents(&config).unwrap(),
                        [] as [QueuedAckIntent; 0]
                    );
                    assert!(journal::read_queued_release_intents(&config).is_err());
                } else {
                    assert!(ack.is_err());
                    assert_eq!(release.unwrap().completed, 1);
                    assert!(journal::read_queued_ack_intents(&config).is_err());
                    assert_eq!(
                        journal::read_queued_release_intents(&config).unwrap(),
                        [] as [QueuedReleaseIntentView; 0]
                    );
                }
                mcp_agent_mail_storage::flush_async_commits();
            });
        }
    }

    #[test]
    fn failed_intents_are_bounded_fair_and_do_not_amplify_the_journal() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            database_url: format!("sqlite://{}", temp.path().join("mail.sqlite3").display()),
            ..Config::default()
        };
        let mut selected = pool_config(&config);
        selected.run_migrations = true;
        let pool = DbPool::new(&selected).unwrap();
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let cx = Cx::current().unwrap();
            let project = queries::ensure_project(&cx, &pool, "/replay")
                .await
                .into_result()
                .unwrap();
            let agent = queries::register_agent(
                &cx,
                &pool,
                project.id.unwrap(),
                "BlueLake",
                "codex-cli",
                "test",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .unwrap();
            let message = queries::create_message_with_recipients(
                &cx,
                &pool,
                project.id.unwrap(),
                agent.id.unwrap(),
                "valid last",
                "body",
                None,
                "normal",
                true,
                "[]",
                &[(agent.id.unwrap(), "to")],
            )
            .await
            .into_result()
            .unwrap();
            for offset in 0..=MAX_ATTEMPTS {
                journal::append_ack_intent(
                    &config,
                    "/missing-project",
                    "BlueLake",
                    i64::try_from(offset).unwrap() + 1000,
                    "test",
                    "busy",
                    None,
                )
                .unwrap();
            }
            journal::append_ack_intent(
                &config,
                "/replay",
                "BlueLake",
                message.id.unwrap(),
                "test",
                "busy",
                None,
            )
            .unwrap();
            let intents = journal::read_queued_ack_intents(&config).unwrap();
            let path = journal::log_path(&config, journal::ACK_INTENT_LOG_FILE);
            let before = std::fs::read(&path).unwrap();
            let stop = AtomicBool::new(true);
            let mut cursor = RoundCursor::default();
            let stopped = replay_ack_batch(&cx, &pool, &config, &mut cursor, &stop, &intents)
                .await
                .unwrap();
            assert!(stopped.interrupted);
            assert_eq!(stopped.attempted, 0);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            stop.store(false, Ordering::Release);

            let foreign_path = temp.path().join("other.sqlite3");
            std::fs::write(&foreign_path, b"not the live mailbox").unwrap();
            let foreign = Config {
                database_url: format!("sqlite://{}", foreign_path.display()),
                ..config.clone()
            };
            assert!(
                replay_ack_batch(&cx, &pool, &foreign, &mut cursor, &stop, &intents)
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);

            let first = replay_ack_batch(&cx, &pool, &config, &mut cursor, &stop, &intents)
                .await
                .unwrap();
            assert_eq!(first.attempted, MAX_ATTEMPTS);
            assert_eq!(first.deferred, MAX_ATTEMPTS);
            assert_eq!(first.completed, 0);
            assert!(first.more);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "no repeated failure records"
            );
            let second = replay_ack_batch(&cx, &pool, &config, &mut cursor, &stop, &intents)
                .await
                .unwrap();
            assert_eq!(
                (second.attempted, second.completed, second.deferred),
                (2, 1, 1)
            );
            assert_eq!(
                journal::read_queued_ack_intents(&config).unwrap().len(),
                MAX_ATTEMPTS + 1
            );
            // A missing identity was not silently created by replay.
            let conn = pool.acquire(&cx).await.into_result().unwrap();
            let rows = conn
                .query_sync("SELECT COUNT(*) AS count FROM projects", &[])
                .unwrap();
            assert_eq!(rows[0].get_named::<i64>("count").unwrap(), 1);
        });
    }

    #[test]
    fn successful_partial_pages_catch_up_without_accelerating_failed_or_idle_work() {
        let now = Instant::now();
        for (applied, completed, more, deferred, interrupted, fast) in [
            (1, 0, true, 0, false, true),
            (0, 1, true, 0, false, true),
            (1, 0, true, 1, false, false),
            (1, 0, true, 0, true, false),
            (0, 0, true, 0, false, false),
            (1, 1, false, 0, false, false),
        ] {
            let mut lane = ScanLane::new(journal_scan::Kind::Release);
            lane.next_scan = Some(now + POLL_INTERVAL);
            lane.replay_finished(
                &ReplayReport {
                    applied,
                    completed,
                    deferred,
                    more,
                    interrupted,
                    ..ReplayReport::default()
                },
                now,
            );
            assert_eq!(
                lane.next_delay(now),
                if fast {
                    CATCH_UP_INTERVAL
                } else {
                    POLL_INTERVAL
                }
            );
        }
    }
}
