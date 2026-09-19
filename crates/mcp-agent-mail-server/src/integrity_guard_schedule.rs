//! Scheduling state for the integrity worker's automatic recovery work.
//!
//! Failed exports preserve evidence. Retrying them on every five-minute quick
//! cycle can therefore exhaust disk even when the live mailbox is healthy
//! (GH#326). Both automatic backup entry points must share one retry window.
//! This limits retry amplification; it is not a disk quota, a cross-process
//! lock, or a repair for an export/validation failure.
//!
//! A full-check failure is separately sticky: time passing or a weaker quick
//! check cannot authorize backups, reconciliation, or database maintenance.

use std::time::{Duration, Instant};

pub(super) const BACKUP_RETRY_INITIAL: Duration = Duration::from_secs(15 * 60);
pub(super) const BACKUP_RETRY_MAX: Duration = Duration::from_secs(6 * 60 * 60);
const FULL_RETRY_INITIAL: Duration = Duration::from_secs(5 * 60);
const FULL_RETRY_MAX: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Default)]
struct RetryWindow {
    failures: u32,
    completed_at: Option<Instant>,
    delay: Duration,
}

impl RetryWindow {
    fn remaining(&self, now: Instant) -> Duration {
        self.completed_at.map_or(Duration::ZERO, |completed| {
            self.delay
                .saturating_sub(now.saturating_duration_since(completed))
        })
    }

    fn fail(&mut self, now: Instant, initial: Duration, maximum: Duration) {
        self.failures = self.failures.saturating_add(1);
        let exponent = self.failures.saturating_sub(1).min(31);
        self.delay = initial.saturating_mul(1_u32 << exponent).min(maximum);
        self.completed_at = Some(now);
    }

    fn defer(&mut self, now: Instant, minimum: Duration) {
        // A skipped attempt supplies no new healthy evidence. Keep the
        // failure count and its delay, even if the producer returned Ok(None).
        self.delay = self.delay.max(minimum);
        self.completed_at = Some(now);
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// One worker's sticky full-verification requirement and bounded retry pacing.
#[derive(Debug, Default)]
pub(super) struct FullVerificationGate {
    required: bool,
    retry: RetryWindow,
}

impl FullVerificationGate {
    /// Require full evidence after a scheduled probe, a corruption refusal,
    /// or a failed quick cycle. Requiring it again never resets its backoff.
    pub(super) fn require(&mut self) {
        self.required = true;
    }

    #[must_use]
    pub(super) const fn is_required(&self) -> bool {
        self.required
    }

    #[must_use]
    pub(super) fn retry_remaining(&self, now: Instant) -> Duration {
        self.retry.remaining(now)
    }

    /// Record completion, not start, so a slow failed probe cannot consume
    /// its retry delay while it is still running.
    pub(super) fn complete(&mut self, now: Instant, passed: bool) {
        if passed {
            self.required = false;
            self.retry.clear();
        } else {
            self.required = true;
            self.retry.fail(now, FULL_RETRY_INITIAL, FULL_RETRY_MAX);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackupKind {
    Proactive,
    Verified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackupCompletion {
    Published,
    Skipped,
    Failed,
}

/// Shared pacing for both automatic backup calls on one mailbox worker.
/// Explicit operator backup calls are deliberately outside this scheduler.
#[derive(Debug, Default)]
pub(super) struct AutomaticBackupSchedule {
    retry: RetryWindow,
    verification_pending: bool,
}

impl AutomaticBackupSchedule {
    pub(super) fn request_verified(&mut self) {
        // A new hourly/daily request cannot bypass a previous failed export.
        self.verification_pending = true;
    }

    #[must_use]
    pub(super) fn next_attempt(&self, now: Instant) -> Option<BackupKind> {
        if !self.retry.remaining(now).is_zero() {
            return None;
        }
        Some(if self.verification_pending {
            BackupKind::Verified
        } else {
            BackupKind::Proactive
        })
    }

    #[must_use]
    pub(super) fn retry_remaining(&self, now: Instant) -> Duration {
        self.retry.remaining(now)
    }

    #[must_use]
    pub(super) const fn consecutive_failures(&self) -> u32 {
        self.retry.failures
    }

    pub(super) fn complete(&mut self, kind: BackupKind, outcome: BackupCompletion, now: Instant) {
        match outcome {
            BackupCompletion::Published => {
                self.retry.clear();
                if kind == BackupKind::Verified {
                    self.verification_pending = false;
                }
            }
            BackupCompletion::Failed => {
                self.retry.fail(now, BACKUP_RETRY_INITIAL, BACKUP_RETRY_MAX);
            }
            BackupCompletion::Skipped => {
                if kind == BackupKind::Verified || self.retry.failures > 0 {
                    // A verified snapshot may be skipped because its fresh
                    // live check failed. Keep that request pending, but do not
                    // hammer another full scan on every quick cycle either.
                    self.retry.defer(now, BACKUP_RETRY_INITIAL);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_backup_is_proactive_and_immediately_eligible() {
        let schedule = AutomaticBackupSchedule::default();
        assert_eq!(
            schedule.next_attempt(Instant::now()),
            Some(BackupKind::Proactive)
        );
        assert_eq!(schedule.consecutive_failures(), 0);
    }

    #[test]
    fn verified_requests_do_not_bypass_a_failed_proactive_export() {
        let now = Instant::now();
        let mut schedule = AutomaticBackupSchedule::default();
        schedule.complete(BackupKind::Proactive, BackupCompletion::Failed, now);
        schedule.request_verified();
        assert_eq!(schedule.next_attempt(now), None);
        assert_eq!(
            schedule
                .next_attempt(now + BACKUP_RETRY_INITIAL.saturating_sub(Duration::from_nanos(1))),
            None
        );
        assert_eq!(
            schedule.next_attempt(now + BACKUP_RETRY_INITIAL),
            Some(BackupKind::Verified)
        );
    }

    #[test]
    fn changing_backup_kinds_cannot_reset_exponential_backoff() {
        let start = Instant::now();
        let mut schedule = AutomaticBackupSchedule::default();
        schedule.complete(BackupKind::Proactive, BackupCompletion::Failed, start);
        let retry = start + BACKUP_RETRY_INITIAL;
        schedule.request_verified();
        schedule.complete(BackupKind::Verified, BackupCompletion::Failed, retry);
        assert_eq!(schedule.consecutive_failures(), 2);
        assert_eq!(schedule.retry_remaining(retry), BACKUP_RETRY_INITIAL * 2);
        for _ in 0..100 {
            schedule.request_verified();
        }
        assert_eq!(schedule.next_attempt(retry + BACKUP_RETRY_INITIAL), None);
    }

    #[test]
    fn skipped_verified_snapshot_is_not_publication_or_recovery() {
        let now = Instant::now();
        let mut schedule = AutomaticBackupSchedule::default();
        schedule.request_verified();
        schedule.complete(BackupKind::Verified, BackupCompletion::Failed, now);
        let retry = now + BACKUP_RETRY_INITIAL;
        schedule.complete(BackupKind::Verified, BackupCompletion::Skipped, retry);
        assert_eq!(schedule.consecutive_failures(), 1);
        assert_eq!(schedule.next_attempt(retry), None);
        assert_eq!(
            schedule.next_attempt(retry + BACKUP_RETRY_INITIAL),
            Some(BackupKind::Verified)
        );
    }

    #[test]
    fn clean_verified_skip_is_also_paced_and_remains_pending() {
        let now = Instant::now();
        let mut schedule = AutomaticBackupSchedule::default();
        schedule.request_verified();
        schedule.complete(BackupKind::Verified, BackupCompletion::Skipped, now);
        assert_eq!(schedule.consecutive_failures(), 0);
        assert_eq!(schedule.next_attempt(now), None);
        assert_eq!(
            schedule.next_attempt(now + BACKUP_RETRY_INITIAL),
            Some(BackupKind::Verified)
        );
    }

    #[test]
    fn actual_verified_publication_clears_failure_and_pending_state() {
        let now = Instant::now();
        let mut schedule = AutomaticBackupSchedule::default();
        schedule.request_verified();
        schedule.complete(BackupKind::Verified, BackupCompletion::Failed, now);
        schedule.complete(
            BackupKind::Verified,
            BackupCompletion::Published,
            now + BACKUP_RETRY_INITIAL,
        );
        assert_eq!(schedule.consecutive_failures(), 0);
        assert_eq!(
            schedule.next_attempt(now + BACKUP_RETRY_INITIAL),
            Some(BackupKind::Proactive)
        );
    }

    #[test]
    fn proactive_publication_cannot_discard_a_pending_verified_request() {
        let now = Instant::now();
        let mut schedule = AutomaticBackupSchedule::default();
        schedule.request_verified();
        schedule.complete(BackupKind::Proactive, BackupCompletion::Published, now);
        assert_eq!(schedule.next_attempt(now), Some(BackupKind::Verified));
    }

    #[test]
    fn failure_delay_caps_without_counter_or_duration_wrap() {
        let now = Instant::now();
        let mut schedule = AutomaticBackupSchedule::default();
        schedule.retry.failures = u32::MAX;
        schedule.complete(BackupKind::Proactive, BackupCompletion::Failed, now);
        assert_eq!(schedule.consecutive_failures(), u32::MAX);
        assert_eq!(schedule.retry_remaining(now), BACKUP_RETRY_MAX);
        assert_eq!(
            schedule.next_attempt(now + BACKUP_RETRY_MAX),
            Some(BackupKind::Proactive)
        );
    }

    #[test]
    fn day_of_five_minute_polling_cannot_amplify_into_hundreds_of_failed_copies() {
        let start = Instant::now();
        let mut schedule = AutomaticBackupSchedule::default();
        let mut attempts = Vec::new();
        for minute in (0_u64..24 * 60).step_by(5) {
            let now = start + Duration::from_secs(minute * 60);
            if minute % 60 == 0 {
                schedule.request_verified();
            }
            if let Some(kind) = schedule.next_attempt(now) {
                attempts.push(minute);
                schedule.complete(kind, BackupCompletion::Failed, now);
            }
        }
        assert_eq!(attempts, [0, 15, 45, 105, 225, 465, 825, 1185]);
    }

    #[test]
    fn backoff_is_measured_from_completion_and_cannot_expire_on_a_reversed_clock() {
        let started = Instant::now();
        let completed = started + Duration::from_secs(60 * 60);
        let mut schedule = AutomaticBackupSchedule::default();
        schedule.complete(BackupKind::Proactive, BackupCompletion::Failed, completed);
        assert_eq!(schedule.retry_remaining(started), BACKUP_RETRY_INITIAL);
        assert_eq!(schedule.next_attempt(completed), None);
        assert!(
            schedule
                .next_attempt(completed + BACKUP_RETRY_INITIAL)
                .is_some()
        );
    }

    #[test]
    fn failed_full_verification_remains_required_after_retry_delay_expires() {
        let now = Instant::now();
        let mut gate = FullVerificationGate::default();
        gate.complete(now, false);
        assert!(gate.is_required());
        assert_eq!(gate.retry_remaining(now), FULL_RETRY_INITIAL);
        assert_eq!(
            gate.retry_remaining(now + FULL_RETRY_INITIAL),
            Duration::ZERO
        );
        assert!(gate.is_required(), "elapsed time is not healthy evidence");
        gate.complete(now + FULL_RETRY_INITIAL, true);
        assert!(!gate.is_required());
    }

    #[test]
    fn repeated_full_requests_keep_the_failure_window() {
        let now = Instant::now();
        let mut gate = FullVerificationGate::default();
        gate.complete(now, false);
        for _ in 0..100 {
            gate.require();
        }
        assert_eq!(gate.retry_remaining(now), FULL_RETRY_INITIAL);
        gate.complete(now + FULL_RETRY_INITIAL, false);
        assert_eq!(
            gate.retry_remaining(now + FULL_RETRY_INITIAL),
            FULL_RETRY_INITIAL * 2
        );
        assert!(gate.is_required());
    }

    #[test]
    fn full_probe_failure_backoff_is_bounded_and_success_resets_it() {
        let now = Instant::now();
        let mut gate = FullVerificationGate::default();
        for _ in 0..100 {
            gate.complete(now, false);
        }
        assert_eq!(gate.retry_remaining(now), FULL_RETRY_MAX);
        gate.complete(now + FULL_RETRY_MAX, true);
        gate.require();
        assert_eq!(gate.retry_remaining(now + FULL_RETRY_MAX), Duration::ZERO);
    }

    #[test]
    fn mailbox_workers_do_not_share_failure_or_verification_state() {
        let now = Instant::now();
        let mut failed = AutomaticBackupSchedule::default();
        let healthy = AutomaticBackupSchedule::default();
        failed.complete(BackupKind::Proactive, BackupCompletion::Failed, now);
        failed.request_verified();
        assert_eq!(failed.next_attempt(now), None);
        assert_eq!(healthy.next_attempt(now), Some(BackupKind::Proactive));
        assert_eq!(healthy.consecutive_failures(), 0);
    }
}
