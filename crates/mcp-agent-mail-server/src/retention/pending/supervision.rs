//! Replace failed replay threads without reviving their runtime or cursor state.
//!
//! A worker is replaced only after `is_finished` and join, never because a slow
//! heartbeat expired. In particular, a blocked writer is not detached while it
//! can still mutate the mailbox. Each lane backs off independently after an
//! exit or spawn failure, even if its peer is healthy. Only a sustained run
//! resets the backoff; briefly starting a thread cannot defeat the rate limit.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mcp_agent_mail_core::Config;

use super::journal_scan::Kind;
use super::{
    REPLAY_KINDS, ReplayWorker, ReplayWorkerGroup, replay_kind_name, sleep_until_next_pass,
};

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const STABLE_RUN: Duration = Duration::from_secs(300);
const SUPERVISION_INTERVAL: Duration = Duration::from_secs(1);

/// Retain the original mailbox even when starting the supervisor itself fails.
/// A later server-start call can retry, but cannot silently retarget one lane.
pub(super) struct Service {
    pub(super) config: Config,
    supervisor: Option<Supervisor>,
}

impl Service {
    pub(super) fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
            supervisor: None,
        }
    }

    pub(super) fn ensure_running(&mut self) -> io::Result<()> {
        if self
            .supervisor
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return Ok(());
        }
        // A failed supervisor drops and joins its group during unwinding. Join
        // that supervisor before creating another owner of this mailbox pair.
        drop(self.supervisor.take());
        self.supervisor = Some(Supervisor::spawn(&self.config, ReplayWorker::spawn)?);
        Ok(())
    }
}

struct Supervisor {
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Supervisor {
    fn spawn(
        config: &Config,
        mut spawn: impl FnMut(&Config, Kind) -> io::Result<ReplayWorker> + Send + 'static,
    ) -> io::Result<Self> {
        let config = config.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&shutdown);
        let handle = std::thread::Builder::new()
            .name("degraded-replay-supervisor".to_string())
            .stack_size(mcp_agent_mail_core::worker_stack_size())
            .spawn(move || {
                let mut group = ReplayWorkerGroup {
                    config,
                    slots: [None, None],
                };
                let mut retries = [Retry::default(), Retry::default()];
                loop {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let events = tick(&mut group, &mut retries, &stop, Instant::now(), &mut spawn);
                    for event in events {
                        match event {
                            Event::Exited { kind, delay } => tracing::warn!(
                                kind = replay_kind_name(kind),
                                retry_after_secs = delay.as_secs(),
                                "durable replay worker exited; joined before scheduled replacement"
                            ),
                            Event::SpawnFailed { kind, delay, error } => tracing::warn!(
                                kind = replay_kind_name(kind), retry_after_secs = delay.as_secs(),
                                %error, "durable replay worker could not start; peer retained"
                            ),
                            Event::Started { kind, replacement } => tracing::info!(
                                kind = replay_kind_name(kind),
                                replacement,
                                "durable replay worker started with fresh journal admission state"
                            ),
                        }
                    }
                    if sleep_until_next_pass(SUPERVISION_INTERVAL, &stop) {
                        break;
                    }
                }
                // The group's Drop signals BOTH children before joining either,
                // also if this supervisor unwinds. No child is ever orphaned.
            })?;
        Ok(Self {
            shutdown,
            handle: Some(handle),
        })
    }

    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            // Deliberately retain shutdown ownership across blocking child I/O.
            // Releasing the owner early would permit overlapping replay groups.
            let _ = handle.join();
        }
    }
}

#[derive(Default)]
struct Retry {
    backoff: Duration,
    next_attempt: Option<Instant>,
    running_since: Option<Instant>,
}

impl Retry {
    fn due(&self, now: Instant) -> bool {
        self.next_attempt.is_none_or(|next| now >= next)
    }

    fn started(&mut self, now: Instant) {
        self.running_since = Some(now);
        self.next_attempt = None;
    }

    fn failed(&mut self, now: Instant) -> Duration {
        if self
            .running_since
            .take()
            .is_some_and(|started| now.saturating_duration_since(started) >= STABLE_RUN)
        {
            self.backoff = Duration::ZERO;
        }
        self.backoff = if self.backoff.is_zero() {
            INITIAL_BACKOFF
        } else {
            self.backoff.saturating_mul(2).min(MAX_BACKOFF)
        };
        self.next_attempt = Some(now + self.backoff);
        self.backoff
    }
}

#[derive(Debug)]
enum Event {
    Exited {
        kind: Kind,
        delay: Duration,
    },
    SpawnFailed {
        kind: Kind,
        delay: Duration,
        error: io::Error,
    },
    Started {
        kind: Kind,
        replacement: bool,
    },
}

/// The caller supplies a monotonic clock so backoff boundaries can be tested
/// without sleeps. There are exactly two retained retry states, and a tick
/// attempts at most one spawn per lane. No global lifecycle mutex is acquired.
fn tick(
    group: &mut ReplayWorkerGroup,
    retries: &mut [Retry; 2],
    shutdown: &AtomicBool,
    now: Instant,
    spawn: &mut impl FnMut(&Config, Kind) -> io::Result<ReplayWorker>,
) -> Vec<Event> {
    let mut events = Vec::new();
    for ((slot, retry), kind) in group.slots.iter_mut().zip(retries).zip(REPLAY_KINDS) {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        if slot.as_ref().is_some_and(ReplayWorker::is_finished) {
            drop(slot.take());
            events.push(Event::Exited {
                kind,
                delay: retry.failed(now),
            });
        }
        if slot.is_none() && retry.due(now) {
            match spawn(&group.config, kind) {
                Ok(worker) => {
                    *slot = Some(worker);
                    let replacement = !retry.backoff.is_zero();
                    retry.started(now);
                    events.push(Event::Started { kind, replacement });
                }
                Err(error) => events.push(Event::SpawnFailed {
                    kind,
                    delay: retry.failed(now),
                    error,
                }),
            }
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn group() -> ReplayWorkerGroup {
        ReplayWorkerGroup {
            config: Config::default(),
            slots: [None, None],
        }
    }

    fn sleeper(_: &Config, kind: Kind) -> io::Result<ReplayWorker> {
        ReplayWorker::spawn_with(kind, |stop| {
            assert!(sleep_until_next_pass(Duration::from_secs(3600), stop));
        })
    }

    fn wait_for_exit(worker: &ReplayWorker) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() {
            assert!(Instant::now() < deadline, "worker did not finish");
            std::thread::yield_now();
        }
    }

    #[test]
    fn restart_backoff_is_capped_and_short_lived_success_does_not_reset_it() {
        let mut retry = Retry::default();
        let mut now = Instant::now();
        assert!(retry.due(now));
        for expected in [1, 2, 4, 8, 16, 32, 60, 60, 60] {
            let delay = retry.failed(now);
            assert_eq!(delay, Duration::from_secs(expected));
            let just_before = (now + delay)
                .checked_sub(Duration::from_nanos(1))
                .expect("an instant after a positive delay");
            assert!(!retry.due(just_before));
            now += delay;
            assert!(retry.due(now));
            retry.started(now);
            now += Duration::from_millis(1);
        }
        retry.started(now);
        now += STABLE_RUN;
        assert_eq!(retry.failed(now), INITIAL_BACKOFF);
    }

    #[test]
    fn spawn_failure_retries_without_a_start_call_and_keeps_its_healthy_peer() {
        let mut group = group();
        let mut retries = [Retry::default(), Retry::default()];
        let stop = AtomicBool::new(false);
        let now = Instant::now();
        let first = tick(
            &mut group,
            &mut retries,
            &stop,
            now,
            &mut |config, kind| match kind {
                Kind::Ack => Err(io::Error::other("injected spawn failure")),
                Kind::Release => sleeper(config, kind),
            },
        );
        assert!(matches!(
            &first[0],
            Event::SpawnFailed {
                kind: Kind::Ack,
                ..
            }
        ));
        assert!(group.slots[0].is_none());
        let peer = Arc::clone(&group.slots[1].as_ref().unwrap().shutdown);
        tick(&mut group, &mut retries, &stop, now, &mut |_, _| {
            panic!("backoff must prevent a second spawn in the same tick")
        });
        let mut attempts = 0;
        let after_backoff = tick(
            &mut group,
            &mut retries,
            &stop,
            now + INITIAL_BACKOFF,
            &mut |config, kind| {
                assert!(matches!(kind, Kind::Ack));
                attempts += 1;
                sleeper(config, kind)
            },
        );
        assert_eq!(attempts, 1);
        assert!(matches!(
            &after_backoff[0],
            Event::Started {
                kind: Kind::Ack,
                replacement: true
            }
        ));
        assert!(Arc::ptr_eq(
            &peer,
            &group.slots[1].as_ref().unwrap().shutdown
        ));
        assert!(!peer.load(Ordering::Acquire));
    }

    #[test]
    fn exited_worker_is_joined_before_replacement_with_a_fresh_stop_flag() {
        let mut group = group();
        let mut retries = [Retry::default(), Retry::default()];
        let stop = AtomicBool::new(false);
        let now = Instant::now();
        group.slots[0] =
            Some(ReplayWorker::spawn_with(Kind::Ack, |_| panic!("injected replay panic")).unwrap());
        retries[0].started(now);
        let old_stop = Arc::clone(&group.slots[0].as_ref().unwrap().shutdown);
        wait_for_exit(group.slots[0].as_ref().unwrap());
        group.slots[1] = Some(sleeper(&group.config, Kind::Release).unwrap());
        let peer = Arc::clone(&group.slots[1].as_ref().unwrap().shutdown);
        let observed = tick(&mut group, &mut retries, &stop, now, &mut |_, _| {
            panic!("an exited lane must back off before spawning")
        });
        assert!(matches!(
            &observed[0],
            Event::Exited {
                kind: Kind::Ack,
                ..
            }
        ));
        assert!(group.slots[0].is_none());
        assert!(
            old_stop.load(Ordering::Acquire),
            "finished worker was joined and stopped"
        );
        tick(
            &mut group,
            &mut retries,
            &stop,
            now + INITIAL_BACKOFF,
            &mut sleeper,
        );
        let fresh = &group.slots[0].as_ref().unwrap().shutdown;
        assert!(!Arc::ptr_eq(&old_stop, fresh));
        assert!(!fresh.load(Ordering::Acquire));
        assert!(Arc::ptr_eq(
            &peer,
            &group.slots[1].as_ref().unwrap().shutdown
        ));
    }

    #[test]
    fn cancellation_prevents_launching_the_second_lane_during_a_tick() {
        let mut group = group();
        let mut retries = [Retry::default(), Retry::default()];
        let stop = AtomicBool::new(false);
        let mut attempts = 0;
        tick(
            &mut group,
            &mut retries,
            &stop,
            Instant::now(),
            &mut |config, kind| {
                attempts += 1;
                let worker = sleeper(config, kind)?;
                stop.store(true, Ordering::Release);
                Ok(worker)
            },
        );
        assert_eq!(attempts, 1);
        assert!(group.slots[1].is_none());
        let first_stop = Arc::clone(&group.slots[0].as_ref().unwrap().shutdown);
        drop(group);
        assert!(first_stop.load(Ordering::Acquire));
    }

    #[test]
    fn supervisor_shutdown_joins_both_children_and_preserves_the_selected_config() {
        let config = Config {
            database_url: "sqlite:///selected.sqlite3".into(),
            ..Config::default()
        };
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let exited = Arc::new(AtomicUsize::new(0));
        let worker_exited = Arc::clone(&exited);
        let supervisor = Supervisor::spawn(&config, move |selected, kind| {
            assert_eq!(selected.database_url, "sqlite:///selected.sqlite3");
            let exited = Arc::clone(&worker_exited);
            let started = started_tx.clone();
            ReplayWorker::spawn_with(kind, move |stop| {
                started.send(()).unwrap();
                assert!(sleep_until_next_pass(Duration::from_secs(3600), stop));
                exited.fetch_add(1, Ordering::SeqCst);
            })
        })
        .unwrap();
        for _ in 0..2 {
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        drop(supervisor);
        assert_eq!(
            exited.load(Ordering::SeqCst),
            2,
            "drop returned only after both children exited"
        );
    }

    fn isolated_database_test() -> bool {
        const CHILD: &str = "AM_TEST_REPLAY_SUPERVISION_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named libtest thread");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .output()
            .expect("run isolated replay supervision test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "isolated {name} failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    #[test]
    fn panic_after_database_ack_before_receipt_recovers_without_another_start_or_client_call() {
        if isolated_database_test() {
            return;
        }
        use asupersync::Cx;
        use fastmcp::prelude::McpContext;
        use fastmcp_core::block_on;
        use mcp_agent_mail_db::{DbPool, queries};
        use mcp_agent_mail_tools::degraded_intents as journal;

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
            let mut selected = super::super::pool_config(&config);
            selected.run_migrations = true;
            let pool = DbPool::new(&selected).unwrap();
            let cx = Cx::for_testing();
            let conn = block_on(pool.acquire(&cx)).into_result().unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'supervision', '/supervision', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1)").unwrap();
            drop(conn);
            let message = block_on(queries::create_message_with_recipients(
                &cx,
                &pool,
                71,
                81,
                "survive receipt-boundary panic",
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
            let claim = journal::AckIntentIdempotency {
                key: "supervised-retry".into(),
                fingerprint: mcp_agent_mail_tools::idempotency::compute_fingerprint(
                    "acknowledge_message",
                    &[
                        ("agent", "BlueLake".into()),
                        ("message_id", message_id.to_string()),
                    ],
                ),
            };
            journal::append_ack_intent(
                &config,
                "supervision",
                "BlueLake",
                message_id,
                "test",
                "database unavailable",
                Some(&claim),
            )
            .unwrap();
            let log = journal::log_path(&config, journal::ACK_INTENT_LOG_FILE);
            let before = std::fs::read(&log).unwrap();

            let ack_starts = Arc::new(AtomicUsize::new(0));
            let release_starts = Arc::new(AtomicUsize::new(0));
            let ack_count = Arc::clone(&ack_starts);
            let release_count = Arc::clone(&release_starts);
            let (applied_tx, applied_rx) = std::sync::mpsc::channel();
            let (crash_tx, crash_rx) = std::sync::mpsc::channel();
            let mut crash_rx = Some(crash_rx);
            let supervisor = Supervisor::spawn(&config, move |selected, kind| {
                if matches!(kind, Kind::Release) {
                    release_count.fetch_add(1, Ordering::SeqCst);
                    return ReplayWorker::spawn(selected, kind);
                }
                if ack_count.fetch_add(1, Ordering::SeqCst) != 0 {
                    return ReplayWorker::spawn(selected, kind);
                }
                let selected = selected.clone();
                let applied = applied_tx.clone();
                let crash = crash_rx.take().unwrap();
                ReplayWorker::spawn_with(kind, move |_| {
                    let pool = DbPool::new(&super::super::pool_config(&selected)).unwrap();
                    let intents = journal::read_queued_ack_intents(&selected).unwrap();
                    assert_eq!(intents.len(), 1);
                    let first_ack = block_on(async {
                        let cx = Cx::current().unwrap();
                        let _lease = mcp_agent_mail_db::write_barrier::begin_write_activity();
                        super::super::validate_live_pool(&cx, &pool, &selected).await.unwrap();
                        let ctx = McpContext::new(cx.clone(), 0);
                        assert!(matches!(super::super::apply_ack(&ctx, &pool, &intents[0]).await.unwrap(),
                            super::super::Completion::Replayed));
                        let conn = pool.acquire(&cx).await.into_result().unwrap();
                        conn.query_sync("SELECT ack_ts FROM message_recipients WHERE message_id = ? AND agent_id = 81",
                            &[message_id.into()]).unwrap()[0].get_named::<i64>("ack_ts").unwrap()
                    });
                    applied.send(first_ack).unwrap();
                    // The DB operation above is real. Deliberately omit the
                    // journal completion, then let this entire thread unwind.
                    crash.recv_timeout(Duration::from_secs(20)).unwrap();
                    panic!("injected crash after DB commit and before completion receipt");
                })
            }).unwrap();
            let original_ack = applied_rx.recv_timeout(Duration::from_secs(20)).unwrap();
            assert!(original_ack > 0);
            assert_eq!(
                std::fs::read(&log).unwrap(),
                before,
                "the first worker wrote no completion"
            );
            assert_eq!(journal::read_queued_ack_intents(&config).unwrap().len(), 1);
            crash_tx.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            let completed = loop {
                if journal::read_queued_ack_intents(&config).is_ok_and(|items| items.is_empty()) {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            drop(supervisor);
            assert!(
                completed,
                "the supervisor did not recover the durable ACK after a worker panic"
            );
            assert_eq!(ack_starts.load(Ordering::SeqCst), 2);
            assert_eq!(
                release_starts.load(Ordering::SeqCst),
                1,
                "healthy peer was not restarted"
            );
            let conn = block_on(pool.acquire(&cx)).into_result().unwrap();
            let rows = conn
                .query_sync(
                    "SELECT ack_ts FROM message_recipients WHERE message_id = ? AND agent_id = 81",
                    &[message_id.into()],
                )
                .unwrap();
            assert_eq!(rows[0].get_named::<i64>("ack_ts").unwrap(), original_ack);
            drop(conn);
            let journal_bytes = std::fs::read_to_string(&log).unwrap();
            let records: Vec<serde_json::Value> = journal_bytes
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                records.len(),
                2,
                "only the original intent and its terminal receipt remain"
            );
            assert_eq!(records[1]["status"], journal::REPLAY_STATUS_REPLAYED);
            assert!(!records[1].to_string().contains("supervised-retry"));
        });
    }
}
