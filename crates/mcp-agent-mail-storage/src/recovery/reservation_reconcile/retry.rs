//! Retained, source-revalidated retries for terminal reservation publication.
//!
//! Scan positions may pass a busy archive only while its ID remains in this
//! bounded queue. Conflicting evidence is never retried as a lock failure.
//! Nothing here retains an artifact payload or authorizes an active lease.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use super::*;

const MAX_RETRY_IDS: usize = 1024;
const RETRY_IDS_PER_BATCH: usize = 8;
const RETRY_REPAIRS_PER_BATCH: usize = 2;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct DeferredRelease {
    id: i64,
    attempts: u32,
    ready_at: Instant,
}

#[derive(Debug, Default)]
pub(super) struct RetryQueue {
    pending: VecDeque<DeferredRelease>,
}

impl RetryQueue {
    fn contains(&self, id: i64) -> bool {
        self.pending.iter().any(|retry| retry.id == id)
    }

    fn is_full(&self) -> bool {
        self.pending.len() >= MAX_RETRY_IDS
    }

    fn forget(&mut self, id: i64) {
        self.pending.retain(|retry| retry.id != id);
    }

    fn defer(&mut self, id: i64, now: Instant) -> bool {
        let previous = self.pending.iter().position(|retry| retry.id == id);
        if previous.is_none() && self.is_full() {
            return false;
        }
        let attempts = previous
            .and_then(|index| self.pending.remove(index))
            .map_or(0, |retry| retry.attempts.saturating_add(1));
        let backoff = if attempts == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(1_u64 << attempts.saturating_sub(1).min(6)).min(MAX_BACKOFF)
        };
        self.pending.push_back(DeferredRelease {
            id,
            attempts,
            ready_at: now + backoff,
        });
        true
    }

    fn due(&self, now: Instant) -> Vec<i64> {
        self.pending
            .iter()
            .filter(|retry| retry.ready_at <= now)
            .take(RETRY_IDS_PER_BATCH)
            .map(|retry| retry.id)
            .collect()
    }
}

fn transient(error: &StorageError) -> bool {
    matches!(
        error,
        StorageError::LockContention { .. }
            | StorageError::LockTimeout(_)
            | StorageError::GitIndexLock { .. }
    ) || matches!(error, StorageError::Io(error)
        if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

fn finish(mut report: ReservationReconcileReport, queue: &RetryQueue) -> ReservationReconcileReport {
    report.retry_len = queue.pending.len();
    report
}

fn advance(position: &mut ScanPosition, id: i64) {
    position.after = id;
    if position.ceiling == Some(id) {
        position.ceiling = None;
    }
}

/// Return false on cancellation, without consuming the candidate or its debt.
fn attempt(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    id: i64,
    queue: &mut RetryQueue,
    report: &mut ReservationReconcileReport,
    stop: &AtomicBool,
) -> bool {
    let mut attempted = false;
    // Source validation failures remain conflicts. In particular, an invalid
    // generation or absent holder must not become trusted retry authority.
    let result = read_source(cx, pool, id).map_err(invalid).and_then(|source| {
        source.map_or(Ok(false), |source| {
            let result = publish_release(
                cx,
                pool,
                config,
                &source,
                ReleaseTargets::WithOwnedAliases,
                &mut attempted,
            );
            tracing::info!(target: "maintenance", event = "reservation_release_reconcile",
                reservation_id = id, project = %source.project_slug,
                generation = ?source.generation, db_state = "released",
                action = "verify_terminal_release", outcome = ?result,
                "reservation archive release reconciliation decision");
            result
        })
    });
    report.attempted += usize::from(attempted);
    if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
        report.interrupted = true;
        return false;
    }
    report.scanned += 1;
    match result {
        Ok(repaired) => {
            if repaired {
                report.repaired += 1;
            } else {
                report.unchanged += 1;
            }
            queue.forget(id);
        }
        Err(error) => {
            report.deferred += 1;
            let retryable = transient(&error);
            if retryable {
                report.deferred_transient += 1;
                if !queue.defer(id, Instant::now()) {
                    // The caller reserves capacity before attempting a fresh
                    // ID. Keep this defensive path no-loss if that changes.
                    report.budget_exhausted = true;
                    return false;
                }
            } else {
                queue.forget(id);
            }
            tracing::warn!(target: "maintenance", event = "reservation_release_reconcile_deferred",
                reservation_id = id, reason = %error, transient = retryable,
                "reservation source and conflicting archive evidence retained");
        }
    }
    true
}

pub(super) fn run(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReservationReconcileCursor,
    stop: &AtomicBool,
) -> Result<ReservationReconcileReport, String> {
    let mut report = ReservationReconcileReport::default();
    if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
        report.interrupted = true;
        return Ok(finish(report, &cursor.retries));
    }
    let _write_activity =
        mcp_agent_mail_db::write_barrier::try_begin_write_activity().ok_or_else(|| {
            "reservation reconciliation deferred: recovery promotion or admission contention"
                .to_string()
        })?;
    validate_source(pool, config)?;
    if corruption_circuit_breaker().is_tripped() {
        return Err("reservation reconciliation refused: corruption breaker open".into());
    }
    // Retry work precedes ordinary selection, so it must independently pass
    // the same writable-source admission before touching any archive path.
    {
        let conn = outcome(block_on(pool.acquire(cx)))?;
        let mode = conn
            .query_sync("PRAGMA query_only", &[])
            .map_err(source_error)?;
        if mode.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
            return Err("query-only snapshots cannot authorize reservation archive repair".into());
        }
    }
    let identity = pool.sqlite_identity_key();
    if identity != cursor.source_identity {
        *cursor = ReservationReconcileCursor {
            source_identity: identity,
            ..Default::default()
        };
    }
    let due = cursor.retries.due(Instant::now());
    let mut selected_count = i64::try_from(due.len()).expect("bounded retry selection");
    let mut seen = HashSet::new();
    for id in due {
        if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
            report.interrupted = true;
            return Ok(finish(report, &cursor.retries));
        }
        if report.attempted >= RETRY_REPAIRS_PER_BATCH {
            report.budget_exhausted = true;
            break;
        }
        if corruption_circuit_breaker().is_tripped() {
            return Err("reservation reconciliation stopped: source corruption observed".into());
        }
        if !attempt(cx, pool, config, id, &mut cursor.retries, &mut report, stop) {
            return Ok(finish(report, &cursor.retries));
        }
        seen.insert(id);
    }
    // Preserve both new guard-unblocking work and historical progress even
    // under permanent retry pressure: at least one write slot for each lane.
    let priority_limit = (report.attempted + PRIORITY_REPAIRS_PER_BATCH)
        .min(MAX_REPAIRS_PER_BATCH - 1);
    for unexpired_only in [true, false] {
        let position = if unexpired_only {
            &mut cursor.priority
        } else {
            &mut cursor.history
        };
        let remaining = IDS_PER_BATCH - selected_count;
        let id_limit = if unexpired_only {
            PRIORITY_IDS_PER_BATCH.min(remaining)
        } else {
            remaining
        };
        if id_limit == 0 {
            break;
        }
        let repair_limit = if unexpired_only {
            priority_limit
        } else {
            MAX_REPAIRS_PER_BATCH
        };
        let ids = select_ids(cx, pool, position, id_limit, unexpired_only)?;
        selected_count += i64::try_from(ids.len()).expect("bounded selection");
        for id in ids {
            if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
                report.interrupted = true;
                return Ok(finish(report, &cursor.retries));
            }
            // The retained lane owns its cooldown and turn. Scan duplicates
            // may advance because their debt remains recorded, never evicted.
            if seen.contains(&id) || cursor.retries.contains(id) {
                advance(position, id);
                continue;
            }
            if report.attempted >= repair_limit || cursor.retries.is_full() {
                report.budget_exhausted = true;
                break;
            }
            if corruption_circuit_breaker().is_tripped() {
                return Err("reservation reconciliation stopped: source corruption observed".into());
            }
            if !attempt(cx, pool, config, id, &mut cursor.retries, &mut report, stop) {
                return Ok(finish(report, &cursor.retries));
            }
            seen.insert(id);
            advance(position, id);
        }
    }
    // Consuming a healthy page is discovery progress, not proof that the
    // mailbox is settled. A lost release can lie behind thousands of already
    // archived or still-active rows. Continue this finite round at catch-up
    // cadence instead of sleeping a minute after every unchanged page.
    // Conflicts alone do not accelerate scanning, and reaching the captured
    // ceiling (or an empty final page) returns to the normal idle cadence.
    if report.deferred == 0
        && report.scanned > 0
        && (cursor.priority.ceiling.is_some() || cursor.history.ceiling.is_some())
    {
        report.budget_exhausted = true;
    }
    Ok(finish(report, &cursor.retries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_deferral_is_due_next_pass_and_repeated_contention_backs_off() {
        let now = Instant::now();
        let mut queue = RetryQueue::default();
        assert!(queue.defer(401, now));
        assert_eq!(queue.due(now), [401]);
        assert!(queue.defer(401, now));
        assert!(queue.due(now).is_empty());
        assert!(queue.due(now + Duration::from_millis(999)).is_empty());
        assert_eq!(queue.due(now + Duration::from_secs(1)), [401]);
        for _ in 0..100 {
            assert!(queue.defer(401, now));
        }
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(queue.pending[0].ready_at, now + MAX_BACKOFF);
        queue.forget(401);
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn full_queue_never_evicts_a_deferred_id_and_due_work_rotates_fairly() {
        let now = Instant::now();
        let mut queue = RetryQueue::default();
        for id in 1..=MAX_RETRY_IDS {
            assert!(queue.defer(i64::try_from(id).unwrap(), now));
        }
        assert!(!queue.defer(50_000, now));
        assert_eq!(queue.pending.len(), MAX_RETRY_IDS);
        assert!(queue.contains(1));
        let due = queue.due(now);
        assert_eq!(due, (1..=8).collect::<Vec<_>>());
        for id in due {
            assert!(queue.defer(id, now));
        }
        assert_eq!(queue.due(now), (9..=16).collect::<Vec<_>>());
        queue.forget(9);
        assert!(queue.defer(50_000, now));
        assert_eq!(queue.pending.len(), MAX_RETRY_IDS);
        assert!(queue.contains(1));
    }

    #[test]
    fn lock_and_io_backpressure_are_distinct_from_conflicting_or_cancelled_evidence() {
        assert!(transient(&StorageError::LockTimeout("held".into())));
        assert!(transient(&std::io::Error::from(std::io::ErrorKind::WouldBlock).into()));
        assert!(transient(&std::io::Error::from(std::io::ErrorKind::TimedOut).into()));
        assert!(!transient(&invalid("foreign generation")));
        assert!(!transient(&std::io::Error::from(std::io::ErrorKind::Interrupted).into()));
        assert!(!transient(&std::io::Error::from(std::io::ErrorKind::PermissionDenied).into()));
    }

    #[test]
    fn healthy_history_pages_find_a_lost_release_without_idle_pauses() {
        super::super::tests::fixture(|cx, pool, config| {
            let stop = AtomicBool::new(false);
            let initial = run(
                cx,
                pool,
                config,
                &mut ReservationReconcileCursor::default(),
                &stop,
            )
            .unwrap();
            assert_eq!(initial.repaired, 1);
            assert!(!initial.budget_exhausted);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 402..482 {
                conn.execute_raw(&format!(
                    "INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'healthy/{id}.rs', 1, 'unchanged', 1000000, 9000000, NULL)"
                )).unwrap();
            }
            conn.execute_raw(
                "INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES(482, 71, 81, 'lost/release.rs', 1, 'released', 1000000, 9000000, 5000000)"
            ).unwrap();
            drop(conn);
            let mut cursor = ReservationReconcileCursor::default();
            let path = config
                .storage_root
                .join("projects/project/file_reservations/id-482-gaabb.json");
            for after in [432, 464] {
                let page = run(cx, pool, config, &mut cursor, &stop).unwrap();
                assert_eq!((page.scanned, page.unchanged, page.repaired), (32, 32, 0));
                assert_eq!((page.attempted, page.deferred, page.retry_len), (0, 0, 0));
                assert!(page.budget_exhausted, "unfinished healthy page needs catch-up");
                assert_eq!(cursor.history.after, after);
                assert_eq!(cursor.history.ceiling, Some(482));
                assert!(!path.exists());
            }
            let final_page = run(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!((final_page.scanned, final_page.unchanged, final_page.repaired), (18, 17, 1));
            assert_eq!(final_page.attempted, 1);
            assert!(!final_page.budget_exhausted, "do not spin into another history round");
            assert_eq!(cursor.history.ceiling, None);
            let artifact = read_artifact(&path).unwrap().unwrap();
            assert_eq!(artifact.value["id"], 482);
            assert_eq!(artifact.value["db_generation"], "aabb");
            assert_eq!(timestamp(&artifact.value["released_ts"]), Some(5_000_000));
            let repo = Repository::open(&config.storage_root).unwrap();
            let tree = head_tree(&repo).unwrap().unwrap();
            assert_eq!(
                committed_artifact(
                    &repo,
                    Some(&tree),
                    "projects/project/file_reservations/id-482-gaabb.json",
                )
                .unwrap()
                .unwrap()
                .bytes,
                artifact.bytes,
            );
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert_eq!(
                conn.query_sync("SELECT COUNT(*) FROM file_reservations", &[])
                    .unwrap()[0]
                    .get_as::<i64>(0)
                    .unwrap(),
                82,
            );
            assert_eq!(
                conn.query_sync("SELECT released_ts FROM file_reservations WHERE id=482", &[])
                    .unwrap()[0]
                    .get_as::<i64>(0)
                    .unwrap(),
                5_000_000,
            );
        });
    }

    #[test]
    fn conflicting_history_page_does_not_gain_fast_discovery_cadence() {
        super::super::tests::fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let (path, before) = super::super::tests::seed_artifact(config, &source, "aabb", true);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 402..482 {
                conn.execute_raw(&format!(
                    "INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'healthy/{id}.rs', 1, 'unchanged', 1000000, 9000000, NULL)"
                )).unwrap();
            }
            drop(conn);
            let mut cursor = ReservationReconcileCursor::default();
            let page = run(cx, pool, config, &mut cursor, &AtomicBool::new(false)).unwrap();
            assert_eq!((page.scanned, page.unchanged, page.deferred), (32, 31, 1));
            assert_eq!((page.attempted, page.repaired, page.retry_len), (0, 0, 0));
            assert!(!page.budget_exhausted);
            assert_eq!(cursor.history.after, 432);
            assert_eq!(cursor.history.ceiling, Some(481));
            assert_eq!(fs::read(path).unwrap(), before);
        });
    }

    #[test]
    fn busy_release_is_repaired_before_history_wrap_with_current_live_metadata() {
        super::super::tests::fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 402..482 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'active/{id}.rs', 1, 'unchanged', 1000000, 9000000, NULL)")).unwrap();
            }
            drop(conn);
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let (path, before) = super::super::tests::seed_artifact(config, &source, "aabb", false);
            let archive = crate::ensure_archive(config, "project").unwrap();
            let process = crate::archive_process_lock(&archive).unwrap();
            let held = process.lock().unwrap();
            let mut cursor = ReservationReconcileCursor::default();
            let stop = AtomicBool::new(false);
            let first = run(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(first.scanned, 32);
            assert_eq!(first.deferred_transient, 1);
            assert_eq!(first.retry_len, 1);
            assert_eq!(first.repaired, 0);
            assert_eq!(cursor.history.after, 432);
            assert_eq!(cursor.history.ceiling, Some(481));
            assert_eq!(fs::read(&path).unwrap(), before);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE file_reservations SET reason='after contention' WHERE id=401").unwrap();
            drop(conn);
            drop(held);
            let resumed = run(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(resumed.repaired, 1, "must not wait for the history round to wrap");
            assert_eq!(resumed.retry_len, 0);
            assert!(cursor.history.after > 432 && cursor.history.after < 481);
            assert!(resumed.scanned <= usize::try_from(IDS_PER_BATCH).unwrap());
            assert!(resumed.attempted <= MAX_REPAIRS_PER_BATCH);
            let artifact = read_artifact(&path).unwrap().unwrap();
            assert_eq!(artifact.value["reason"], "after contention");
            assert_eq!(artifact.value["operator_note"], "preserve this note");
            assert_eq!(artifact.value["released_ts"], source.artifact["released_ts"]);
            let repo = Repository::open(&config.storage_root).unwrap();
            let tree = head_tree(&repo).unwrap().unwrap();
            assert_eq!(committed_artifact(&repo, Some(&tree), "projects/project/file_reservations/id-401-gaabb.json").unwrap().unwrap().bytes, artifact.bytes);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert_eq!(conn.query_sync("SELECT released_ts FROM file_reservations WHERE id=401", &[]).unwrap()[0].get_as::<Option<i64>>(0).unwrap(), None);
            assert_eq!(conn.query_sync("SELECT released_ts FROM file_reservation_releases WHERE reservation_id=401", &[]).unwrap()[0].get_as::<i64>(0).unwrap(), 5_000_000);
            assert_eq!(conn.query_sync("SELECT COUNT(*) FROM file_reservations", &[]).unwrap()[0].get_as::<i64>(0).unwrap(), 81);
        });
    }

    #[test]
    fn saturated_cooling_queue_does_not_consume_unretained_scan_work() {
        super::super::tests::fixture(|cx, pool, config| {
            let mut cursor = ReservationReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                ..Default::default()
            };
            let now = Instant::now();
            for id in 1..=MAX_RETRY_IDS {
                let id = 10_000 + i64::try_from(id).unwrap();
                assert!(cursor.retries.defer(id, now));
                cursor.retries.pending.back_mut().unwrap().ready_at = now + MAX_BACKOFF;
            }
            let report = run(cx, pool, config, &mut cursor, &AtomicBool::new(false)).unwrap();
            assert_eq!(report.scanned, 0);
            assert_eq!(report.attempted, 0);
            assert_eq!(report.retry_len, MAX_RETRY_IDS);
            assert!(report.budget_exhausted);
            assert_eq!(cursor.history.after, 0);
            assert!(!config.storage_root.join("projects").exists());
        });
    }

    #[test]
    fn retry_debt_cannot_bypass_shutdown_or_query_only_admission() {
        super::super::tests::fixture(|cx, pool, config| {
            let mut cursor = ReservationReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                ..Default::default()
            };
            assert!(cursor.retries.defer(401, Instant::now()));
            let stopped = run(cx, pool, config, &mut cursor, &AtomicBool::new(true)).unwrap();
            assert!(stopped.interrupted);
            assert_eq!(stopped.scanned, 0);
            assert_eq!(stopped.retry_len, 1);
            let readonly = DbPool::new_query_only(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            }).unwrap();
            assert!(run(cx, &readonly, config, &mut cursor, &AtomicBool::new(false)).unwrap_err().contains("query-only"));
            assert!(cursor.retries.contains(401));
            assert!(!config.storage_root.join("projects").exists());
        });
    }

    #[test]
    fn retained_retries_leave_write_slots_for_unexpired_releases_and_history() {
        super::super::tests::fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let future = mcp_agent_mail_db::now_micros() + 86_400_000_000;
            for (id, expires) in [(402, 9_000_000), (403, 9_000_000), (900, future), (901, future)] {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'retry/{id}.rs', 1, 'released', 1000000, {expires}, 5000000)")).unwrap();
            }
            drop(conn);
            let mut cursor = ReservationReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                ..Default::default()
            };
            assert!(cursor.retries.defer(401, Instant::now()));
            assert!(cursor.retries.defer(402, Instant::now()));
            let report = run(cx, pool, config, &mut cursor, &AtomicBool::new(false)).unwrap();
            assert_eq!(report.repaired, 4);
            assert_eq!(report.attempted, 4);
            assert_eq!(report.scanned, 4, "scan duplicates must not be attempted twice");
            assert_eq!(report.retry_len, 0);
            let dir = config.storage_root.join("projects/project/file_reservations");
            for id in [401, 402, 403, 900] {
                assert!(dir.join(reservation_artifact_filename(Some("aabb"), id)).is_file());
            }
            assert!(!dir.join(reservation_artifact_filename(Some("aabb"), 901)).exists());
        });
    }

    #[test]
    fn conflicting_retry_withdraws_debt_without_overwriting_evidence() {
        super::super::tests::fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let (path, before) = super::super::tests::seed_artifact(config, &source, "aabb", true);
            let mut cursor = ReservationReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                ..Default::default()
            };
            assert!(cursor.retries.defer(401, Instant::now()));
            let report = run(cx, pool, config, &mut cursor, &AtomicBool::new(false)).unwrap();
            assert_eq!(report.deferred, 1);
            assert_eq!(report.deferred_transient, 0);
            assert_eq!(report.retry_len, 0);
            assert_eq!(report.attempted, 0);
            assert_eq!(report.repaired, 0);
            assert_eq!(fs::read(path).unwrap(), before);
        });
    }
}
