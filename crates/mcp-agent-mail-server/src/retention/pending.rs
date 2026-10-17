//! Replay durable closeout intents without requiring the original client to return.
//!
//! This worker is independent of archive maintenance and destructive retention.
//! Independent, incremental ACK and release scans admit no mutation before a
//! complete, handle-validated snapshot. A scan step consumes at most 256 KiB or
//! 256 records; snapshots, retained identities and pending payloads have explicit
//! admission bounds. At most sixteen acknowledgements and sixteen release pages
//! of up to 64 candidate rows follow per pass. These are row/work bounds, not
//! hard deadlines on filesystem I/O or SQLite's internal query execution.
//! Failed intents remain queued without another failure record every tick.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
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

const MAX_ATTEMPTS: usize = 16;
const POLL_INTERVAL: Duration = Duration::from_secs(30);
const CATCH_UP_INTERVAL: Duration = Duration::from_secs(1);
const SCAN_INTERVAL: Duration = Duration::from_millis(100);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER: LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    LazyLock::new(|| Mutex::new(None));

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

struct SnapshotReaders {
    acknowledgements: ScanLane,
    releases: ScanLane,
}

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

pub(super) fn start(config: &Config) {
    // There is no persistent mailbox to recover for an in-memory database.
    if mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url).is_none()
    {
        return;
    }
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if worker
        .as_ref()
        .is_some_and(std::thread::JoinHandle::is_finished)
        && let Some(stale) = worker.take()
    {
        let _ = stale.join();
    }
    if worker.is_some() {
        return;
    }
    let config = config.clone();
    SHUTDOWN.store(false, Ordering::Release);
    match std::thread::Builder::new()
        .name("degraded-intent-replay".to_string())
        .stack_size(mcp_agent_mail_core::worker_stack_size())
        .spawn(move || run(&config))
    {
        Ok(handle) => *worker = Some(handle),
        Err(error) => tracing::warn!(%error, "could not start durable intent replay worker"),
    }
}

pub(super) fn shutdown() {
    // Serialize stop/start so a restart cannot clear the old worker's flag
    // before that worker has joined. The worker never takes this mutex.
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    SHUTDOWN.store(true, Ordering::Release);
    if let Some(handle) = worker.take() {
        let _ = handle.join();
    }
}

fn sleep_until_next_pass(duration: Duration) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if SHUTDOWN.load(Ordering::Acquire) {
            return true;
        }
        let step = remaining.min(Duration::from_millis(100));
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
    SHUTDOWN.load(Ordering::Acquire)
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

fn run(config: &Config) {
    let mut cursors = ReplayCursors::default();
    let mut readers = SnapshotReaders::new();
    let mut pool = None;
    // Give startup admission priority; no client request is needed thereafter.
    if sleep_until_next_pass(CATCH_UP_INTERVAL) {
        return;
    }
    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            return;
        }
        let snapshots = readers.poll(config, &SHUTDOWN, Instant::now());
        if !snapshots.needs_db() {
            // Do not retain a connection for an idle, healthy mailbox.
            pool = None;
        } else if pool.is_none() {
            match mcp_agent_mail_db::create_pool(&pool_config(config)) {
                Ok(created) => pool = Some(created),
                Err(error) => tracing::warn!(
                    %error, "durable intent replay awaits live database admission"
                ),
            }
        }
        let results = fastmcp_core::block_on(async {
            match Cx::current() {
                Some(cx) => {
                    replay_snapshots(
                        &cx,
                        pool.as_ref(),
                        config,
                        &mut cursors,
                        &SHUTDOWN,
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
        let mut deferred = false;
        for ((kind, lane), result) in [
            ("acknowledgement", &mut readers.acknowledgements),
            ("release", &mut readers.releases),
        ]
        .into_iter()
        .zip(results)
        {
            match result {
                Ok(report) => {
                    if report.attempted > 0 || report.interrupted {
                        tracing::info!(target: "maintenance", event = "degraded_intent_replay",
                            kind, attempted = report.attempted, applied = report.applied,
                            completed = report.completed, abandoned = report.abandoned,
                            rows_released = report.rows_released, deferred = report.deferred,
                            more = report.more, interrupted = report.interrupted,
                            "durable closeout replay pass completed");
                    }
                    lane.replay_finished(&report, Instant::now());
                    deferred |= report.deferred > 0 || report.interrupted;
                }
                Err(error) => {
                    tracing::warn!(kind, %error,
                        "durable closeout replay deferred; journal preserved");
                    deferred = true;
                }
            }
        }
        if deferred {
            // A repaired database may have replaced the old generation.
            pool = None;
        }
        let interval = readers.next_delay(Instant::now());
        if sleep_until_next_pass(interval) {
            return;
        }
    }
}

/// Drive the same two independent lanes used by the worker. No public tool
/// handler is called: it would recursively drain the journal or requeue errors.
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
