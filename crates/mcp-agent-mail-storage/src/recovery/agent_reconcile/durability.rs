//! Connect bounded profile repair to the shared WBQ durability obligation.
//!
//! Tickets are captured before repair. Completion rereads the writable live
//! source and both exact working/Git artifacts without retaining publication
//! locks across pool checkout. A newer authoritative profile can supersede the
//! failed mutable snapshot, but another incarnation/generation never can.

use std::sync::atomic::AtomicBool;
use std::time::Instant;

use asupersync::Cx;
use fastmcp_core::block_on;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbPool, corruption_circuit_breaker};

use super::retry::Failure;
use super::{
    AgentReconcileCursor, AgentSource, checkpoint, observation, outcome, read_source_connection,
    validate_source,
};
use crate::recovery::message_reconcile::database::durability::profiles::{self, Tickets};

pub(super) fn seed_retries(config: &Config, cursor: &mut AgentReconcileCursor) {
    let now = Instant::now();
    profiles::seed_retries(config, |id| {
        if cursor.retries.contains(id) || !cursor.retries.has_capacity() {
            return false;
        }
        cursor.retries.defer(id, now);
        cursor.next_lane_is_retry = true;
        true
    });
}

fn ready(cx: &Cx, stop: &AtomicBool) -> Result<(), String> {
    checkpoint(cx, stop).map_err(|error| error.to_string())?;
    if corruption_circuit_breaker().is_tripped()
        || mcp_agent_mail_core::global_metrics()
            .system
            .disk_pressure_level
            .load()
            >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64()
    {
        return Err("profile durability proof deferred by source or disk health".into());
    }
    Ok(())
}

/// Every read is from the configured writable serving pool. The bounded
/// generation projection detects an in-place restore as well as file swaps.
fn read_current(cx: &Cx, pool: &DbPool, id: i64) -> Result<(AgentSource, String), String> {
    let conn = outcome(block_on(pool.acquire(cx)))?;
    let read = || -> Result<(AgentSource, String), Failure> {
        let mode = conn
            .query_sync("PRAGMA query_only", &[])
            .map_err(Failure::source_unavailable)?;
        if mode.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
            return Err("query-only snapshots cannot discharge profile durability failure".into());
        }
        #[cfg(test)]
        tests::before_generation_read(&conn);
        let rows = conn
            .query_sync(
                "SELECT singleton, generation_id FROM db_identity \
                 WHERE length(generation_id) <= 256 LIMIT 2",
                &[],
            )
            .map_err(Failure::source_unavailable)?;
        let [row] = rows.as_slice() else {
            return Err("profile durability proof requires one database generation".into());
        };
        if row
            .get_named::<i64>("singleton")
            .map_err(Failure::source_unavailable)?
            != 0
        {
            return Err("profile durability proof generation singleton is invalid".into());
        }
        let generation = row
            .get_named::<String>("generation_id")
            .map_err(Failure::source_unavailable)?;
        let source = read_source_connection(&conn, id)?;
        Ok((source, generation))
    };
    // Limit unwind recovery to the engine reads and decoding. A panicked
    // reader is retired; neither publication nor worker ownership is caught.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(read))
        .unwrap_or_else(|_| {
            Err(Failure::source_unavailable(
                "profile durability source read panicked",
            ))
        });
    if result.as_ref().is_err_and(|error| error.transient) {
        mcp_agent_mail_db::close_db_conn(conn.detach(), "failed profile durability source read");
    }
    result.map_err(|error| error.detail)
}

/// The caller retains its original database-promotion exclusion throughout.
/// A failed proof does not change successful archive-repair counts or forgive
/// any other failure; the independent ticket stays eligible for later repair.
pub(super) fn confirm_repaired(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    source_identity: &str,
    id: i64,
    tickets: &Tickets,
    stop: &AtomicBool,
) {
    if tickets.is_empty() {
        return;
    }
    let verify = || -> Result<(), String> {
        ready(cx, stop)?;
        validate_source(pool, config)?;
        if pool.sqlite_identity_key() != source_identity {
            return Err("profile durability source changed after selection".into());
        }
        let (source, generation) = read_current(cx, pool, id)?;
        if !profiles::matches_current(
            config,
            &generation,
            &source.project_slug,
            &source.profile,
            tickets,
        ) {
            return Err(
                "failed profile incarnation or generation is no longer authoritative".into(),
            );
        }
        let observed = observation::observe_profile(config, &source)
            .ok_or("profile durability proof requires complete working and Git artifacts")?
            .map_err(|error| error.detail)?;
        // Pool admission is outside every archive mutation/project lock. A
        // lifecycle or generation change during that wait invalidates the
        // captured observation rather than approving stale published bytes.
        let (latest, latest_generation) = read_current(cx, pool, id)?;
        ready(cx, stop)?;
        if pool.sqlite_identity_key() != source_identity
            || !profiles::matches_current(
                config,
                &latest_generation,
                &latest.project_slug,
                &latest.profile,
                tickets,
            )
            || !observed
                .matches_source(&latest)
                .map_err(|error| error.to_string())?
            || !observed
                .still_complete()
                .map_err(|error| error.to_string())?
        {
            return Err("profile source or archive changed during durability verification".into());
        }
        ready(cx, stop)?;
        Ok(())
    };
    match verify() {
        Ok(()) => profiles::complete(tickets),
        Err(reason) => {
            profiles::postpone(tickets);
            tracing::warn!(target: "maintenance", event = "wbq_profile_durability_repair_deferred",
                agent_id = id, reason = %reason,
                "failed profile remains unverified; preserving write refusal");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        read_source, reconcile_agent, reconcile_agent_batch,
        tests::{assert_profile_committed, profile_path, with_mailbox},
    };
    use super::*;
    use crate::{WbqEnqueueResult, WbqFlushOutcome, WriteOp};
    use serde_json::json;
    use std::fs;

    std::thread_local! {
        static PANIC_BEFORE_GENERATION_READ: std::cell::Cell<bool> = const {
            std::cell::Cell::new(false)
        };
    }

    pub(super) fn before_generation_read(conn: &mcp_agent_mail_db::DbConn) {
        if PANIC_BEFORE_GENERATION_READ.replace(false) {
            conn.execute_raw("PRAGMA query_only = ON").unwrap();
            panic!("private-profile-generation-read-panic");
        }
    }

    fn failed_profile(cx: &Cx, pool: &DbPool, config: &Config) -> WriteOp {
        let source = read_source(cx, pool, 101).unwrap();
        crate::ensure_archive(config, &source.project_slug).unwrap();
        fs::create_dir_all(profile_path(config)).unwrap();
        let op = WriteOp::AgentProfile {
            project_slug: source.project_slug,
            config: config.clone(),
            agent_json: source.profile,
        };
        enqueue_failure(&op);
        op
    }

    fn enqueue_failure(op: &WriteOp) {
        crate::wbq_start();
        let before = crate::wbq_stats().unrecoverable_errors;
        assert_eq!(crate::wbq_enqueue(op.clone()), WbqEnqueueResult::Enqueued);
        assert_eq!(crate::wbq_flush_status(), WbqFlushOutcome::PersistenceFailed);
        assert_eq!(crate::wbq_stats().unrecoverable_errors, before + 1);
        assert!(crate::durability_degraded());
    }

    fn retain_obstruction(config: &Config, name: &str) {
        fs::rename(profile_path(config), config.storage_root.join(name)).unwrap();
    }

    fn drive(cx: &Cx, pool: &DbPool, config: &Config) -> super::super::AgentReconcileReport {
        reconcile_agent_batch(
            cx,
            pool,
            config,
            &mut AgentReconcileCursor::default(),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    fn update(cx: &Cx, pool: &DbPool, sql: &str) {
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw(sql).unwrap();
    }

    #[test]
    fn profile_generation_read_panic_retires_the_real_failed_connection() {
        with_mailbox(|cx, pool, _, _| {
            PANIC_BEFORE_GENERATION_READ.set(true);
            let error = read_current(cx, pool, 101).expect_err("generation read must panic");
            assert!(error.contains("panicked"), "{error}");
            assert!(!error.contains("private-profile-generation-read-panic"));
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert_eq!(
                conn.query_sync("PRAGMA query_only", &[]).unwrap()[0]
                    .get_as::<i64>(0)
                    .unwrap(),
                0,
                "the failed reader must not return its query-only state to the pool"
            );
        });
    }

    #[test]
    fn first_profile_failure_recovers_from_real_queue_into_verified_git() {
        with_mailbox(|cx, pool, config, _| {
            // The fixture starts with the first checkout of a brand-new file.
            // Initialization must establish this witness before registration
            // could enqueue its first profile; never fabricate one at failure.
            let witness =
                mcp_agent_mail_db::recovery_state::read_revision_witness(&config.storage_root)
                    .unwrap()
                    .expect("successful fresh startup publishes a recovery generation");
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let generation = conn
                .query_sync(
                    "SELECT generation_id FROM db_identity WHERE singleton = 0",
                    &[],
                )
                .unwrap()[0]
                .get_named::<String>("generation_id")
                .unwrap();
            drop(conn);
            assert_eq!(witness.generation(), generation);

            failed_profile(cx, pool, config);
            let failures = crate::wbq_stats().unrecoverable_errors;
            retain_obstruction(config, "retained-first-profile-obstruction");
            update(
                cx,
                pool,
                "UPDATE agents SET last_active_ts = 3000000 WHERE id = 101",
            );
            let mut cursor = AgentReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                after: 999,
                ceiling: Some(999),
                ..Default::default()
            };
            let report = reconcile_agent_batch(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(
                (report.scanned, report.repaired, report.deferred),
                (1, 1, 0)
            );
            assert_eq!(
                assert_profile_committed(config),
                read_source(cx, pool, 101).unwrap().profile
            );
            assert!(!crate::durability_degraded());
            assert_eq!(crate::wbq_stats().unrecoverable_errors, failures);
            assert_eq!(crate::wbq_shutdown_status(), WbqFlushOutcome::Drained);
        });
    }

    #[test]
    fn newer_authoritative_policy_and_unretirement_discharge_older_profile_failure() {
        with_mailbox(|cx, pool, config, _| {
            update(
                cx,
                pool,
                "UPDATE agents SET retired_at = 3000000, contact_policy = 'block_all', \
                 task_description = 'older failed profile' WHERE id = 101",
            );
            let failed = failed_profile(cx, pool, config);
            retain_obstruction(config, "retained-superseded-profile-obstruction");
            update(
                cx,
                pool,
                "UPDATE agents SET retired_at = NULL, contact_policy = 'open', \
                 task_description = 'newer authoritative profile' WHERE id = 101",
            );
            let expected = read_source(cx, pool, 101).unwrap().profile;
            assert_eq!(drive(cx, pool, config).repaired, 1);
            let published = assert_profile_committed(config);
            assert_eq!(published, expected);
            let WriteOp::AgentProfile { agent_json, .. } = failed else {
                unreachable!()
            };
            assert_ne!(published["contact_policy"], agent_json["contact_policy"]);
            assert!(published["retired_at"].is_null());
            assert!(!crate::durability_degraded());
            assert_eq!(crate::wbq_shutdown_status(), WbqFlushOutcome::Drained);
        });
    }

    #[test]
    fn conflicting_profile_evidence_cannot_discharge_the_failed_write() {
        with_mailbox(|cx, pool, config, _| {
            let failed = failed_profile(cx, pool, config);
            retain_obstruction(config, "retained-conflicting-profile-obstruction");
            let WriteOp::AgentProfile {
                mut agent_json, ..
            } = failed
            else {
                unreachable!()
            };
            agent_json["deregistered_at"] = json!(mcp_agent_mail_db::micros_to_iso(3000000));
            let bytes = serde_json::to_vec_pretty(&agent_json).unwrap();
            fs::write(profile_path(config), &bytes).unwrap();
            crate::commit_paths_with_retry(
                &config.storage_root,
                config,
                "test: retain conflicting permanent lifecycle",
                &["projects/project/agents/BlueLake/profile.json"],
            )
            .unwrap();
            let before = git2::Repository::open(&config.storage_root)
                .unwrap()
                .head()
                .unwrap()
                .target();
            let mut cursor = AgentReconcileCursor::default();
            let stop = AtomicBool::new(false);
            let report = reconcile_agent_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(report.repaired, 0);
            assert!(report.deferred > 0);
            let repeated = reconcile_agent_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(repeated.suppressed_conflicts, 1);
            assert_eq!(repeated.deferred, 0);
            assert_eq!(fs::read(profile_path(config)).unwrap(), bytes);
            assert_eq!(
                git2::Repository::open(&config.storage_root)
                    .unwrap()
                    .head()
                    .unwrap()
                    .target(),
                before
            );
            assert!(crate::durability_degraded());
            assert_eq!(
                crate::wbq_shutdown_status(),
                WbqFlushOutcome::PersistenceFailed
            );
        });
    }

    #[test]
    fn same_file_new_generation_cannot_inherit_profile_failure_authority() {
        with_mailbox(|cx, pool, config, _| {
            failed_profile(cx, pool, config);
            retain_obstruction(config, "retained-generation-profile-obstruction");
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "UPDATE db_identity SET generation_id = 'deadbeef' WHERE singleton = 0",
            )
            .unwrap();
            mcp_agent_mail_db::recovery_state::ensure_checkpoint(&*conn, &config.storage_root)
                .unwrap();
            drop(conn);
            assert_eq!(
                mcp_agent_mail_db::recovery_state::read_revision_witness(&config.storage_root)
                    .unwrap()
                    .unwrap()
                    .generation(),
                "deadbeef"
            );
            assert_eq!(drive(cx, pool, config).repaired, 1);
            assert_profile_committed(config);
            assert!(crate::durability_degraded());
            assert_eq!(crate::wbq_shutdown_status(), WbqFlushOutcome::PersistenceFailed);
        });
    }

    #[test]
    fn reused_agent_id_with_another_incarnation_cannot_clear_old_failure() {
        with_mailbox(|cx, pool, config, _| {
            failed_profile(cx, pool, config);
            retain_obstruction(config, "retained-incarnation-profile-obstruction");
            update(
                cx,
                pool,
                "UPDATE agents SET inception_ts = 4000000 WHERE id = 101",
            );
            assert_eq!(drive(cx, pool, config).repaired, 1);
            assert_eq!(
                assert_profile_committed(config)["inception_ts"],
                mcp_agent_mail_db::micros_to_iso(4000000)
            );
            assert!(crate::durability_degraded());
            assert_eq!(crate::wbq_shutdown_status(), WbqFlushOutcome::PersistenceFailed);
        });
    }

    #[test]
    fn lost_permanent_tombstone_cannot_be_forgiven_as_a_newer_profile() {
        with_mailbox(|cx, pool, config, _| {
            update(
                cx,
                pool,
                "INSERT INTO agent_deregistrations(agent_id, deregistered_at) \
                 VALUES(101, 3000000)",
            );
            failed_profile(cx, pool, config);
            retain_obstruction(config, "retained-tombstone-profile-obstruction");
            update(
                cx,
                pool,
                "DELETE FROM agent_deregistrations WHERE agent_id = 101",
            );
            assert_eq!(drive(cx, pool, config).repaired, 1);
            assert!(assert_profile_committed(config)["deregistered_at"].is_null());
            assert!(crate::durability_degraded());
            assert_eq!(crate::wbq_shutdown_status(), WbqFlushOutcome::PersistenceFailed);
        });
    }

    #[test]
    fn verified_profile_cannot_clear_an_unsupported_secret_bearing_failure() {
        with_mailbox(|cx, pool, config, _| {
            let failed = failed_profile(cx, pool, config);
            retain_obstruction(config, "retained-supported-profile-obstruction");
            let WriteOp::AgentProfile { mut agent_json, .. } = failed else {
                unreachable!()
            };
            agent_json["registration_token"] = json!("private-unsupported-profile-credential");
            enqueue_failure(&WriteOp::AgentProfile {
                project_slug: "project".into(),
                config: config.clone(),
                agent_json,
            });
            assert_eq!(drive(cx, pool, config).repaired, 1);
            let published = assert_profile_committed(config);
            assert!(published.get("registration_token").is_none());
            assert!(
                !published
                    .to_string()
                    .contains("private-unsupported-profile-credential")
            );
            assert!(crate::durability_degraded());
            assert_eq!(crate::wbq_shutdown_status(), WbqFlushOutcome::PersistenceFailed);
        });
    }

    #[test]
    fn profile_proof_cannot_discharge_a_failure_recorded_after_ticket_capture() {
        with_mailbox(|cx, pool, config, _| {
            let failed = failed_profile(cx, pool, config);
            retain_obstruction(config, "retained-sequence-first-obstruction");
            let source = read_source(cx, pool, 101).unwrap();
            let tickets = profiles::tickets_for(config, &source.project_slug, &source.profile);
            assert!(!tickets.is_empty());
            let stop = AtomicBool::new(false);
            let activity = mcp_agent_mail_db::write_barrier::try_begin_write_activity().unwrap();
            assert!(reconcile_agent(cx, pool, config, &source, &stop).unwrap());
            drop(activity);
            assert_profile_committed(config);

            // A second real queued write fails after the old proof captured
            // its ticket. Restore the verified bytes without clearing debt.
            let retained = config
                .storage_root
                .join("retained-profile-before-second-failure");
            fs::rename(profile_path(config), &retained).unwrap();
            fs::create_dir(profile_path(config)).unwrap();
            enqueue_failure(&failed);
            retain_obstruction(config, "retained-sequence-second-obstruction");
            fs::rename(retained, profile_path(config)).unwrap();

            let activity = mcp_agent_mail_db::write_barrier::try_begin_write_activity().unwrap();
            confirm_repaired(
                cx,
                pool,
                config,
                &pool.sqlite_identity_key(),
                101,
                &tickets,
                &stop,
            );
            drop(activity);
            assert!(
                crate::durability_degraded(),
                "older ticket cannot forgive newer failure"
            );
            assert_eq!(drive(cx, pool, config).unchanged, 1);
            assert_profile_committed(config);
            assert!(!crate::durability_degraded());
            assert_eq!(crate::wbq_stats().unrecoverable_errors, 2);
            assert_eq!(crate::wbq_shutdown_status(), WbqFlushOutcome::Drained);
        });
    }
}
