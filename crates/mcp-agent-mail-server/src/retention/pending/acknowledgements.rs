//! Generation-safe acknowledgement replay with retryable promotion admission.
//!
//! Recovery must never wait for a promotion that is itself draining writers.
//! A refused admission leaves the durable journal and fairness cursor untouched.
//! An admitted acknowledgement retains its writer lease through the completion
//! receipt: dropping it after the SQL write but before the marker could certify
//! an acknowledgement against a different, restored database generation.

use std::sync::atomic::{AtomicBool, Ordering};

use asupersync::Cx;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbPool, write_barrier};
use mcp_agent_mail_tools::degraded_intents::QueuedAckIntent;

use super::{
    Completion, MAX_ATTEMPTS, ReplayReport, RoundCursor, append_ack_completion, apply_ack,
    validate_live_pool,
};

pub(super) async fn replay_ack_batch(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut RoundCursor,
    shutdown: &AtomicBool,
    intents: &[QueuedAckIntent],
) -> Result<ReplayReport, String> {
    let mut report = ReplayReport::default();
    if shutdown.load(Ordering::Acquire) || cx.checkpoint().is_err() {
        report.interrupted = true;
        return Ok(report);
    }
    if intents.is_empty() {
        return Ok(report);
    }
    // Unlike the bootstrap API, this refuses even a promotion owned by this
    // thread or a timed-out drain retaining exclusion. No source access or
    // cursor change may precede admission. The durable journal supplies retries.
    let write_activity = write_barrier::try_begin_write_activity()
        .ok_or("durable acknowledgement replay deferred: recovery promotion or admission contention")?;
    validate_live_pool(cx, pool, config).await?;
    let keys: Vec<_> = intents
        .iter()
        .map(|intent| (intent.created_ts, intent.content_sha256.clone()))
        .collect();
    let candidates = cursor.candidates(&keys);
    report.more = candidates.len() > MAX_ATTEMPTS;
    let ctx = McpContext::new(cx.clone(), 0);
    let mut diagnostics = Vec::new();
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
                        diagnostics.push((index, true, error.to_string()));
                    }
                }
            }
            Err(error) => {
                report.deferred += 1;
                diagnostics.push((index, false, error));
            }
        }
    }
    drop(write_activity);
    // A logging subscriber may block or inspect recovery state. It must not
    // prolong this batch's counted writer lease after its durable work is done.
    for (index, receipt_failed, error) in diagnostics {
        let intent = &intents[index];
        tracing::warn!(intent_id = %intent.intent_id, receipt_failed, %error,
            "durable acknowledgement remains queued; replay will retry");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_db::queries;
    use mcp_agent_mail_tools::degraded_intents as journal;
    use std::time::{Duration, Instant};

    /// The barrier is process-global. A subprocess excludes unrelated pool
    /// tests, and a watchdog turns the old indefinite wait into a test failure.
    fn isolated() -> bool {
        const CHILD: &str = "AM_TEST_ACK_ADMISSION_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named libtest thread");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let output = tempfile::NamedTempFile::new().unwrap();
        let log = output.reopen().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if started.elapsed() > Duration::from_secs(90) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{name} exceeded its watchdog");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let text = std::fs::read_to_string(output.path()).unwrap();
        assert!(
            status.success() && text.contains("1 passed; 0 failed"),
            "{name}: {text}"
        );
        true
    }

    struct Fixture {
        config: Config,
        pool: DbPool,
        cx: Cx,
        message: i64,
        intents: Vec<QueuedAckIntent>,
        _temp: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let config = Config {
                storage_root: temp.path().to_path_buf(),
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                    &temp.path().join("mail.sqlite3"),
                ),
                ..Config::default()
            };
            let mut selected = super::super::pool_config(&config);
            selected.run_migrations = true;
            let pool = DbPool::new(&selected).unwrap();
            let cx = Cx::for_testing();
            let message = fastmcp_core::block_on(async {
                let project = queries::ensure_project(&cx, &pool, "/replay")
                    .await.into_result().unwrap();
                let agent = queries::register_agent(
                    &cx, &pool, project.id.unwrap(), "BlueLake", "test", "test",
                    None, None, None,
                ).await.into_result().unwrap();
                queries::create_message_with_recipients(
                    &cx, &pool, project.id.unwrap(), agent.id.unwrap(), "queued ack", "body",
                    None, "normal", true, "[]", &[(agent.id.unwrap(), "to")],
                ).await.into_result().unwrap().id.unwrap()
            });
            journal::append_ack_intent(
                &config, "/replay", "BlueLake", message, "test", "busy", None,
            ).unwrap();
            let intents = journal::read_queued_ack_intents(&config).unwrap();
            Self { config, pool, cx, message, intents, _temp: temp }
        }

        fn replay(&self, cursor: &mut RoundCursor, stopped: bool) -> Result<ReplayReport, String> {
            fastmcp_core::block_on(replay_ack_batch(
                &self.cx, &self.pool, &self.config, cursor, &AtomicBool::new(stopped), &self.intents,
            ))
        }

        fn bytes(&self) -> Vec<u8> {
            std::fs::read(journal::log_path(&self.config, journal::ACK_INTENT_LOG_FILE)).unwrap()
        }

        fn acknowledged(&self) -> bool {
            let conn = fastmcp_core::block_on(self.pool.acquire(&self.cx)).into_result().unwrap();
            let rows = conn.query_sync(
                "SELECT ack_ts FROM message_recipients WHERE message_id = ?",
                &[self.message.into()],
            ).unwrap();
            rows[0].get_named::<Option<i64>>("ack_ts").unwrap().is_some_and(|ts| ts > 0)
        }
    }

    #[test]
    fn ack_replay_refuses_owned_and_failed_drain_admission_without_mutation() {
        if isolated() { return; }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let fixture = Fixture::new();
            let before = fixture.bytes();
            let mut cursor = RoundCursor {
                after: Some((-1, "lower".into())),
                ceiling: Some((i64::MAX, "upper".into())),
            };
            for failed_drain in [false, true] {
                let writer = failed_drain.then(write_barrier::begin_write_activity);
                let (promotion, outcome) =
                    write_barrier::acquire_promotion_barrier_draining(Duration::ZERO);
                assert_eq!(matches!(outcome, write_barrier::DrainOutcome::TimedOut { .. }), failed_drain);
                let after = cursor.after.clone();
                let ceiling = cursor.ceiling.clone();
                let error = fixture.replay(&mut cursor, false).unwrap_err();
                assert!(error.contains("admission contention"));
                assert_eq!(cursor.after, after);
                assert_eq!(cursor.ceiling, ceiling);
                assert_eq!(fixture.bytes(), before);
                assert_eq!(write_barrier::active_writer_count(), usize::from(failed_drain));
                // A late writer drain does not turn failed promotion ownership
                // into acknowledgement admission or permission to bypass it.
                drop(writer);
                assert!(fixture.replay(&mut cursor, false).is_err());
                let stopped = fixture.replay(&mut cursor, true).unwrap();
                assert!(stopped.interrupted && stopped.attempted == 0);
                drop(promotion);
                assert!(!fixture.acknowledged());
            }
            let report = fixture.replay(&mut cursor, false).unwrap();
            assert_eq!((report.attempted, report.applied, report.completed), (1, 1, 1));
            assert!(fixture.acknowledged());
            assert!(journal::read_queued_ack_intents(&fixture.config).unwrap().is_empty());
        });
    }

    #[test]
    fn ack_replay_does_not_wait_for_a_foreign_promotion_owner() {
        if isolated() { return; }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let fixture = Fixture::new();
            let before = fixture.bytes();
            std::thread::scope(|scope| {
                let (ready_tx, ready_rx) = std::sync::mpsc::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let owner = scope.spawn(move || {
                    let promotion = write_barrier::try_acquire_promotion_barrier_if_idle().unwrap();
                    ready_tx.send(()).unwrap();
                    let explicitly_released = release_rx.recv_timeout(Duration::from_secs(15)).is_ok();
                    drop(promotion);
                    explicitly_released
                });
                ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let mut cursor = RoundCursor::default();
                let result = fixture.replay(&mut cursor, false);
                let _ = release_tx.send(());
                assert!(owner.join().unwrap(), "replay waited until the owner timed out");
                assert!(result.unwrap_err().contains("admission contention"));
                assert!(cursor.after.is_none() && cursor.ceiling.is_none());
            });
            assert_eq!(fixture.bytes(), before);
            assert!(!fixture.acknowledged());
            assert_eq!(fixture.replay(&mut RoundCursor::default(), false).unwrap().completed, 1);
            assert!(fixture.acknowledged());
        });
    }
}
