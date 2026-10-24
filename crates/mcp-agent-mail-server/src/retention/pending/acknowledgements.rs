//! Generation-safe acknowledgement replay with retryable promotion admission.
//!
//! Recovery must never wait for a promotion that is itself draining writers.
//! A refused admission leaves the durable journal and fairness cursor untouched.
//! An admitted acknowledgement retains its writer lease through the completion
//! receipt: dropping it after the SQL write but before the marker could certify
//! an acknowledgement against a different, restored database generation.
//! Each intent is a separate admitted unit. Promotion can drain between units;
//! a changed promotion epoch ends the batch before any further source access.

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

#[cfg(test)]
std::thread_local! {
    // Fault injection at real boundaries. Neither hook substitutes database
    // mutation, counted admission, or durable journal publication.
    static BEFORE_RECEIPT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static AFTER_ACK_RELEASE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static AFTER_SOURCE_VALIDATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

async fn admit(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    expected_epoch: Option<u64>,
) -> Result<(write_barrier::WriteActivityGuard, u64), String> {
    let activity = write_barrier::try_begin_write_activity().ok_or(
        "durable acknowledgement replay deferred: recovery promotion or admission contention",
    )?;
    let epoch = write_barrier::promotion_epoch();
    if expected_epoch.is_some_and(|expected| expected != epoch) {
        return Err("mailbox promotion ended the ACK replay batch".into());
    }
    validate_live_pool(cx, pool, config).await?;
    #[cfg(test)]
    AFTER_SOURCE_VALIDATION.with(|slot| {
        let hook = slot.borrow_mut().take();
        if let Some(hook) = hook {
            hook();
        }
    });
    Ok((activity, epoch))
}

/// The caller's lease spans BOTH durable effects, including a failed receipt.
/// Return diagnostics instead of invoking subscribers while that lease is held.
async fn replay_one(
    ctx: &McpContext,
    pool: &DbPool,
    config: &Config,
    intent: &QueuedAckIntent,
    _activity: &write_barrier::WriteActivityGuard,
    report: &mut ReplayReport,
) -> Option<(bool, String)> {
    let completion = match apply_ack(ctx, pool, intent).await {
        Ok(completion) => completion,
        Err(error) => return Some((false, error)),
    };
    if matches!(completion, Completion::Replayed) {
        report.applied += 1;
    }
    #[cfg(test)]
    BEFORE_RECEIPT.with(|slot| {
        let hook = slot.borrow_mut().take();
        if let Some(hook) = hook {
            hook();
        }
    });
    if let Err(error) = append_ack_completion(config, intent, completion) {
        return Some((true, error.to_string()));
    }
    report.completed += 1;
    if matches!(completion, Completion::KeyConflict) {
        report.abandoned += 1;
    }
    None
}

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
    let (write_activity, epoch) = admit(cx, pool, config, None).await?;
    let mut initial_activity = Some(write_activity);
    let keys: Vec<_> = intents
        .iter()
        .map(|intent| (intent.created_ts, intent.content_sha256.clone()))
        .collect();
    // Planning may start a new finite round, but only an admitted attempt may
    // publish that change. Cancellation after validation must not reset it.
    let mut planned = RoundCursor {
        after: cursor.after.clone(),
        ceiling: cursor.ceiling.clone(),
    };
    let candidates = planned.candidates(&keys);
    report.more = candidates.len() > MAX_ATTEMPTS;
    let ctx = McpContext::new(cx.clone(), 0);
    let mut diagnostics = Vec::new();
    for index in candidates.into_iter().take(MAX_ATTEMPTS) {
        if shutdown.load(Ordering::Acquire) || ctx.checkpoint().is_err() {
            report.interrupted = true;
            report.more = true;
            break;
        }
        let write_activity = if let Some(activity) = initial_activity.take() {
            activity
        } else {
            match admit(cx, pool, config, Some(epoch)).await {
                Ok((activity, _)) => activity,
                Err(error) => {
                    // Do not consume an unattempted key. The caller discards
                    // its pool and retries the journal after normal backoff.
                    report.deferred += 1;
                    report.more = true;
                    diagnostics.push((index, false, error));
                    break;
                }
            }
        };
        // Admission/source validation may have yielded. Cancellation here must
        // not advance the fairness cursor or start a new acknowledgement.
        if shutdown.load(Ordering::Acquire) || ctx.checkpoint().is_err() {
            report.interrupted = true;
            report.more = true;
            break;
        }
        let intent = &intents[index];
        cursor.ceiling.clone_from(&planned.ceiling);
        cursor.after = Some(keys[index].clone());
        report.attempted += 1;
        if let Some((receipt_failed, error)) =
            replay_one(&ctx, pool, config, intent, &write_activity, &mut report).await
        {
            report.deferred += 1;
            diagnostics.push((index, receipt_failed, error));
        }
        // A successful SQL acknowledgement without a durable marker is still
        // safely replayable. In either case, promotion can now drain this unit.
        drop(write_activity);
        #[cfg(test)]
        AFTER_ACK_RELEASE.with(|slot| {
            let hook = slot.borrow_mut().take();
            if let Some(hook) = hook {
                hook();
            }
        });
    }
    drop(initial_activity);
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
        fn new(key: Option<&str>) -> Self {
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
                    .await
                    .into_result()
                    .unwrap();
                let agent = queries::register_agent(
                    &cx,
                    &pool,
                    project.id.unwrap(),
                    "BlueLake",
                    "test",
                    "test",
                    None,
                    None,
                    None,
                )
                .await
                .into_result()
                .unwrap();
                queries::create_message_with_recipients(
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
                .unwrap()
                .id
                .unwrap()
            });
            let claim = key.map(|key| claim(key, message));
            journal::append_ack_intent(
                &config,
                "/replay",
                "BlueLake",
                message,
                "test",
                "busy",
                claim.as_ref(),
            )
            .unwrap();
            let intents = journal::read_queued_ack_intents(&config).unwrap();
            Self {
                config,
                pool,
                cx,
                message,
                intents,
                _temp: temp,
            }
        }

        fn replay(&self, cursor: &mut RoundCursor, stopped: bool) -> Result<ReplayReport, String> {
            fastmcp_core::block_on(replay_ack_batch(
                &self.cx,
                &self.pool,
                &self.config,
                cursor,
                &AtomicBool::new(stopped),
                &self.intents,
            ))
        }

        fn bytes(&self) -> Vec<u8> {
            std::fs::read(journal::log_path(
                &self.config,
                journal::ACK_INTENT_LOG_FILE,
            ))
            .unwrap()
        }

        fn acknowledged(&self) -> bool {
            self.receipts(self.message).1.is_some_and(|ts| ts > 0)
        }

        fn receipts(&self, id: i64) -> (Option<i64>, Option<i64>) {
            let conn = fastmcp_core::block_on(self.pool.acquire(&self.cx))
                .into_result()
                .unwrap();
            let rows = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = ?",
                    &[id.into()],
                )
                .unwrap();
            (
                rows[0].get_named("read_ts").unwrap(),
                rows[0].get_named("ack_ts").unwrap(),
            )
        }

        fn queue_another(&mut self) {
            let conn = fastmcp_core::block_on(self.pool.acquire(&self.cx))
                .into_result()
                .unwrap();
            let rows = conn
                .query_sync(
                    "SELECT project_id, sender_id FROM messages WHERE id = ?",
                    &[self.message.into()],
                )
                .unwrap();
            let project = rows[0].get_named::<i64>("project_id").unwrap();
            let agent = rows[0].get_named::<i64>("sender_id").unwrap();
            drop(conn);
            let message = fastmcp_core::block_on(queries::create_message_with_recipients(
                &self.cx,
                &self.pool,
                project,
                agent,
                "next queued ack",
                "body",
                None,
                "normal",
                true,
                "[]",
                &[(agent, "to")],
            ))
            .into_result()
            .unwrap()
            .id
            .unwrap();
            journal::append_ack_intent(
                &self.config,
                "/replay",
                "BlueLake",
                message,
                "test",
                "busy",
                None,
            )
            .unwrap();
            self.intents = journal::read_queued_ack_intents(&self.config).unwrap();
            self.intents.sort_unstable_by(|left, right| {
                (left.created_ts, &left.content_sha256)
                    .cmp(&(right.created_ts, &right.content_sha256))
            });
        }
    }

    fn claim(key: &str, message: i64) -> journal::AckIntentIdempotency {
        journal::AckIntentIdempotency {
            key: key.into(),
            fingerprint: mcp_agent_mail_tools::idempotency::compute_fingerprint(
                "acknowledge_message",
                &[
                    ("agent", "BlueLake".into()),
                    ("message_id", message.to_string()),
                ],
            ),
        }
    }

    #[test]
    fn ack_replay_refuses_owned_and_failed_drain_admission_without_mutation() {
        if isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let fixture = Fixture::new(None);
            let before = fixture.bytes();
            let mut cursor = RoundCursor {
                after: Some((-1, "lower".into())),
                ceiling: Some((i64::MAX, "upper".into())),
            };
            for failed_drain in [false, true] {
                let writer = failed_drain.then(write_barrier::begin_write_activity);
                let (promotion, outcome) =
                    write_barrier::acquire_promotion_barrier_draining(Duration::ZERO);
                assert_eq!(
                    matches!(outcome, write_barrier::DrainOutcome::TimedOut { .. }),
                    failed_drain
                );
                let after = cursor.after.clone();
                let ceiling = cursor.ceiling.clone();
                let error = fixture.replay(&mut cursor, false).unwrap_err();
                assert!(error.contains("admission contention"));
                assert_eq!(cursor.after, after);
                assert_eq!(cursor.ceiling, ceiling);
                assert_eq!(fixture.bytes(), before);
                assert_eq!(
                    write_barrier::active_writer_count(),
                    usize::from(failed_drain)
                );
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
            assert_eq!(
                (report.attempted, report.applied, report.completed),
                (1, 1, 1)
            );
            assert!(fixture.acknowledged());
            assert_eq!(
                journal::read_queued_ack_intents(&fixture.config).unwrap(),
                Vec::new()
            );
        });
    }

    #[test]
    fn ack_replay_does_not_wait_for_a_foreign_promotion_owner() {
        if isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let fixture = Fixture::new(None);
            let before = fixture.bytes();
            std::thread::scope(|scope| {
                let (ready_tx, ready_rx) = std::sync::mpsc::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let owner = scope.spawn(move || {
                    let promotion = write_barrier::try_acquire_promotion_barrier_if_idle().unwrap();
                    ready_tx.send(()).unwrap();
                    let explicitly_released =
                        release_rx.recv_timeout(Duration::from_secs(15)).is_ok();
                    drop(promotion);
                    explicitly_released
                });
                ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let mut cursor = RoundCursor::default();
                let result = fixture.replay(&mut cursor, false);
                let _ = release_tx.send(());
                assert!(
                    owner.join().unwrap(),
                    "replay waited until the owner timed out"
                );
                assert!(result.unwrap_err().contains("admission contention"));
                assert!(cursor.after.is_none() && cursor.ceiling.is_none());
            });
            assert_eq!(fixture.bytes(), before);
            assert!(!fixture.acknowledged());
            assert_eq!(
                fixture
                    .replay(&mut RoundCursor::default(), false)
                    .unwrap()
                    .completed,
                1
            );
            assert!(fixture.acknowledged());
        });
    }

    #[test]
    fn ack_replay_yields_to_promotion_between_receipts_and_resumes_the_unattempted_key() {
        if isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let mut fixture = Fixture::new(None);
            fixture.queue_another();
            let first = fixture.intents[0].message_id;
            let second = fixture.intents[1].message_id;
            let held = std::rc::Rc::new(std::cell::RefCell::new(None));
            let captured = std::rc::Rc::clone(&held);
            AFTER_ACK_RELEASE.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    assert_eq!(write_barrier::active_writer_count(), 0);
                    *captured.borrow_mut() = Some(
                        write_barrier::try_acquire_promotion_barrier_if_idle()
                            .expect("a completed ACK must not retain the whole batch's lease"),
                    );
                }));
            });
            let mut cursor = RoundCursor::default();
            let report = fixture.replay(&mut cursor, false).unwrap();
            assert_eq!(
                (report.attempted, report.completed, report.deferred),
                (1, 1, 1)
            );
            assert!(report.more);
            assert_eq!(
                cursor.after,
                Some((
                    fixture.intents[0].created_ts,
                    fixture.intents[0].content_sha256.clone(),
                ))
            );
            assert_eq!(
                journal::read_queued_ack_intents(&fixture.config)
                    .unwrap()
                    .len(),
                1
            );
            drop(
                held.borrow_mut()
                    .take()
                    .expect("promotion stayed held through refusal"),
            );
            let first_receipts = fixture.receipts(first);
            assert!(first_receipts.0.is_some() && first_receipts.1.is_some());
            assert_eq!(fixture.receipts(second), (None, None));
            let resumed = fixture.replay(&mut cursor, false).unwrap();
            assert_eq!(
                (resumed.attempted, resumed.completed, resumed.deferred),
                (1, 1, 0)
            );
            assert_eq!(fixture.receipts(first), first_receipts);
            assert!(fixture.receipts(second).1.is_some());
            assert_eq!(
                journal::read_queued_ack_intents(&fixture.config).unwrap(),
                Vec::new()
            );
        });
    }

    #[test]
    fn ack_replay_observes_a_completed_promotion_epoch_between_intents() {
        if isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let mut fixture = Fixture::new(None);
            fixture.queue_another();
            let first = fixture.intents[0].message_id;
            let second = fixture.intents[1].message_id;
            let path = std::path::PathBuf::from(fixture.pool.sqlite_path());
            let epoch = write_barrier::promotion_epoch();
            AFTER_ACK_RELEASE.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    let promotion = write_barrier::try_acquire_promotion_barrier_if_idle().unwrap();
                    // Exercise the production promotion-epoch contract, not a
                    // fake admission function. This is not file-swap coverage.
                    write_barrier::record_promotion(&path);
                    drop(promotion);
                }));
            });
            let mut cursor = RoundCursor::default();
            let report = fixture.replay(&mut cursor, false).unwrap();
            assert!(write_barrier::promotion_epoch() > epoch);
            assert_eq!(
                (report.attempted, report.completed, report.deferred),
                (1, 1, 1)
            );
            assert!(report.more);
            assert!(fixture.receipts(first).1.is_some());
            assert_eq!(fixture.receipts(second), (None, None));
            assert_eq!(
                journal::read_queued_ack_intents(&fixture.config)
                    .unwrap()
                    .len(),
                1
            );
            // Match run(): a deferred pass discards its pool before retrying.
            fixture.pool = DbPool::new(&super::super::pool_config(&fixture.config)).unwrap();
            let resumed = fixture.replay(&mut cursor, false).unwrap();
            assert_eq!((resumed.attempted, resumed.completed), (1, 1));
            assert!(fixture.receipts(second).1.is_some());
        });
    }

    #[test]
    fn ack_receipt_lock_failure_preserves_the_keyed_ack_for_idempotent_recovery() {
        if isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let fixture = Fixture::new(Some("original-key"));
            let before = fixture.bytes();
            let lock_path = journal::log_path(&fixture.config, journal::ACK_INTENT_LOG_FILE)
                .with_file_name(journal::ACK_INTENT_LOCK_FILE);
            let held = std::rc::Rc::new(std::cell::RefCell::new(None));
            let captured = std::rc::Rc::clone(&held);
            BEFORE_RECEIPT.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    assert_eq!(write_barrier::active_writer_count(), 1);
                    assert!(write_barrier::try_acquire_promotion_barrier_if_idle().is_none());
                    let lock = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(lock_path)
                        .unwrap();
                    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
                    *captured.borrow_mut() = Some(lock);
                }));
            });
            let report = fixture.replay(&mut RoundCursor::default(), false).unwrap();
            assert_eq!(
                (report.applied, report.completed, report.deferred),
                (1, 0, 1)
            );
            assert_eq!(write_barrier::active_writer_count(), 0);
            assert_eq!(
                fixture.bytes(),
                before,
                "no false completion or new failure record"
            );
            let first = fixture.receipts(fixture.message);
            assert!(first.0.is_some() && first.1.is_some());
            let queued = journal::read_queued_ack_intents(&fixture.config).unwrap();
            assert_eq!(queued[0].idempotency, fixture.intents[0].idempotency);
            drop(
                held.borrow_mut()
                    .take()
                    .expect("receipt lock remained held"),
            );
            // A fresh cursor models losing all in-memory replay progress.
            let retry = fixture.replay(&mut RoundCursor::default(), false).unwrap();
            assert_eq!((retry.applied, retry.completed, retry.deferred), (1, 1, 0));
            assert_eq!(fixture.receipts(fixture.message), first);
            assert_eq!(
                journal::read_queued_ack_intents(&fixture.config).unwrap(),
                Vec::new()
            );
        });
    }

    #[test]
    fn ack_replay_unwind_after_sql_drops_admission_without_certifying_completion() {
        if isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let fixture = Fixture::new(Some("unwind-key"));
            let before = fixture.bytes();
            BEFORE_RECEIPT.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(|| panic!("interrupted after committed ACK")));
            });
            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fixture.replay(&mut RoundCursor::default(), false)
            }));
            assert!(interrupted.is_err());
            assert_eq!(write_barrier::active_writer_count(), 0);
            assert_eq!(fixture.bytes(), before);
            let first = fixture.receipts(fixture.message);
            assert!(first.0.is_some() && first.1.is_some());
            let promotion = write_barrier::try_acquire_promotion_barrier_if_idle().unwrap();
            drop(promotion);
            assert_eq!(
                fixture
                    .replay(&mut RoundCursor::default(), false)
                    .unwrap()
                    .completed,
                1
            );
            assert_eq!(fixture.receipts(fixture.message), first);
            assert_eq!(
                journal::read_queued_ack_intents(&fixture.config).unwrap(),
                Vec::new()
            );
        });
    }

    #[test]
    fn ack_replay_finishes_its_current_receipt_on_shutdown_but_does_not_start_the_next() {
        if isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let mut fixture = Fixture::new(None);
            fixture.queue_another();
            let first = fixture.intents[0].message_id;
            let second = fixture.intents[1].message_id;
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let captured = std::sync::Arc::clone(&stop);
            BEFORE_RECEIPT.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    captured.store(true, Ordering::Release);
                }));
            });
            let mut cursor = RoundCursor::default();
            let report = fastmcp_core::block_on(replay_ack_batch(
                &fixture.cx,
                &fixture.pool,
                &fixture.config,
                &mut cursor,
                &stop,
                &fixture.intents,
            ))
            .unwrap();
            assert_eq!((report.attempted, report.completed), (1, 1));
            assert!(report.interrupted && report.more);
            assert_eq!(write_barrier::active_writer_count(), 0);
            assert!(fixture.receipts(first).1.is_some());
            assert_eq!(fixture.receipts(second), (None, None));
            assert_eq!(
                journal::read_queued_ack_intents(&fixture.config)
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(fixture.replay(&mut cursor, false).unwrap().completed, 1);
            assert!(fixture.receipts(second).1.is_some());
        });
    }

    #[test]
    fn ack_replay_cancelled_after_source_validation_does_not_reset_a_finished_round() {
        if isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let fixture = Fixture::new(None);
            let before = fixture.bytes();
            let key = (
                fixture.intents[0].created_ts,
                fixture.intents[0].content_sha256.clone(),
            );
            let mut cursor = RoundCursor {
                after: Some(key.clone()),
                ceiling: Some(key.clone()),
            };
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let captured = std::sync::Arc::clone(&stop);
            AFTER_SOURCE_VALIDATION.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    captured.store(true, Ordering::Release);
                }));
            });
            let report = fastmcp_core::block_on(replay_ack_batch(
                &fixture.cx,
                &fixture.pool,
                &fixture.config,
                &mut cursor,
                &stop,
                &fixture.intents,
            ))
            .unwrap();
            assert!(report.interrupted);
            assert_eq!((report.attempted, report.completed), (0, 0));
            assert_eq!(cursor.after, Some(key.clone()));
            assert_eq!(cursor.ceiling, Some(key));
            assert_eq!(write_barrier::active_writer_count(), 0);
            assert_eq!(fixture.bytes(), before);
            assert!(!fixture.acknowledged());
            assert_eq!(fixture.replay(&mut cursor, false).unwrap().completed, 1);
            assert!(fixture.acknowledged());
        });
    }
}
