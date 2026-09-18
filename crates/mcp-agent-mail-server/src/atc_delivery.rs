//! Passive liveness observation and bounded admission for actionable ATC mail.
//!
//! Routine liveness checks are NOT messages. A timer, restart, newer activity
//! epoch, or explicit Live executor mode must not turn them into durable
//! `messages`, `message_recipients`, contact requests, or archive work (GH264).
//! The engine already receives attributed tool activity and retains its bounded
//! decision ledger; unsent checks are not acknowledgments or delivery outcomes.
//!
//! Only actionable conflict notifications use this delivery budget/cache.
//! Reservation mutations and their outcome notices bypass this optional-mail
//! gate. The gate records admission, not successful delivery.

use std::collections::HashMap;

pub(crate) const MAX_NOTIFICATIONS_PER_TICK: usize = 16;
const MAX_NOTIFICATION_KEYS: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationClass {
    Probe,
    Liveness,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// An actionable notification may reach the durable executor.
    Admitted,
    /// Observed only in memory; MUST NOT reach the mail/experience executor.
    Passive,
    Duplicate,
    NoActivity,
    RecentlyActive,
    Deferred,
    Capacity,
}

/// Process-local counters. Passive checks never create replacement DB events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AtcDeliveryStats {
    pub admitted: u64,
    pub passive_liveness: u64,
    pub duplicate: u64,
    pub no_activity: u64,
    pub recently_active: u64,
    pub deferred: u64,
    pub capacity_suppressed: u64,
    pub tracked_keys: usize,
}

pub(crate) struct Notification<'a> {
    /// Project-, agent-, and family-scoped semantic key for actionable mail.
    pub key: &'a str,
    pub class: NotificationClass,
    pub last_activity_micros: Option<i64>,
    pub cooldown_micros: i64,
}

#[derive(Debug)]
pub(crate) struct NotificationAdmission {
    cooldowns: HashMap<String, i64>,
    limit: usize,
    key_limit: usize,
    remaining: usize,
    min_probe_silence_micros: i64,
    stats: AtcDeliveryStats,
}

impl NotificationAdmission {
    pub(crate) fn new(min_probe_silence_micros: i64) -> Self {
        Self::with_limits(
            min_probe_silence_micros,
            MAX_NOTIFICATIONS_PER_TICK,
            MAX_NOTIFICATION_KEYS,
        )
    }

    fn with_limits(min_probe_silence_micros: i64, limit: usize, key_limit: usize) -> Self {
        Self {
            cooldowns: HashMap::new(),
            limit,
            key_limit,
            remaining: 0,
            min_probe_silence_micros: min_probe_silence_micros.max(0),
            stats: AtcDeliveryStats::default(),
        }
    }

    pub(crate) fn begin_tick(&mut self, now_micros: i64) {
        self.remaining = self.limit;
        self.cooldowns.retain(|_, until| *until > now_micros);
    }

    pub(crate) fn admit(&mut self, notification: Notification<'_>, now_micros: i64) -> Admission {
        if matches!(notification.class, NotificationClass::Probe | NotificationClass::Liveness) {
            let Some(activity) = notification.last_activity_micros.filter(|ts| *ts > 0) else {
                return self.record(Admission::NoActivity);
            };
            if notification.class == NotificationClass::Probe
                && (activity >= now_micros
                    || now_micros.saturating_sub(activity) < self.min_probe_silence_micros)
            {
                return self.record(Admission::RecentlyActive);
            }
            // No admission entitlement, key allocation, cooldown, or budget is
            // consumed. In particular, a fresh process/epoch still sends zero
            // routine mail, and a large liveness roster cannot starve conflicts.
            return self.record(Admission::Passive);
        }

        if self.cooldowns.get(notification.key).is_some_and(|until| now_micros < *until) {
            return self.record(Admission::Duplicate);
        }
        if self.remaining == 0 {
            // A later proposal can retry; deferral consumes no cooldown.
            return self.record(Admission::Deferred);
        }
        if !self.cooldowns.contains_key(notification.key) && self.cooldowns.len() >= self.key_limit {
            return self.record(Admission::Capacity);
        }
        self.cooldowns.insert(
            notification.key.to_owned(),
            now_micros.saturating_add(notification.cooldown_micros.max(1)),
        );
        self.remaining -= 1;
        self.record(Admission::Admitted)
    }

    fn record(&mut self, admission: Admission) -> Admission {
        let count = match admission {
            Admission::Admitted => &mut self.stats.admitted,
            Admission::Passive => &mut self.stats.passive_liveness,
            Admission::Duplicate => &mut self.stats.duplicate,
            Admission::NoActivity => &mut self.stats.no_activity,
            Admission::RecentlyActive => &mut self.stats.recently_active,
            Admission::Deferred => &mut self.stats.deferred,
            Admission::Capacity => &mut self.stats.capacity_suppressed,
        };
        *count = count.saturating_add(1);
        admission
    }

    pub(crate) fn stats(&self) -> AtcDeliveryStats {
        AtcDeliveryStats { tracked_keys: self.cooldowns.len(), ..self.stats }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const SECOND: i64 = 1_000_000;

    fn probe(key: &str, activity: Option<i64>) -> Notification<'_> {
        Notification {
            key,
            class: NotificationClass::Probe,
            last_activity_micros: activity,
            cooldown_micros: 120 * SECOND,
        }
    }

    fn conflict(key: &str) -> Notification<'_> {
        Notification {
            key,
            class: NotificationClass::Conflict,
            last_activity_micros: None,
            cooldown_micros: 300 * SECOND,
        }
    }

    #[test]
    fn ten_thousand_ticks_emit_no_unanswered_probe_mail() {
        let mut gate = NotificationAdmission::new(120 * SECOND);
        for tick in 0..10_000 {
            let now = (200 + tick * 120) * SECOND;
            gate.begin_tick(now);
            assert_eq!(gate.admit(probe("probe:p:a", Some(SECOND)), now), Admission::Passive);
        }
        assert_eq!(gate.stats().admitted, 0);
        assert_eq!(gate.stats().passive_liveness, 10_000);
        assert_eq!(gate.stats().tracked_keys, 0);
    }

    #[test]
    fn new_activity_and_restarts_do_not_create_delivery_entitlements() {
        for _restart in 0..100 {
            let mut gate = NotificationAdmission::new(0);
            gate.begin_tick(10 * SECOND);
            for activity in [SECOND, 2 * SECOND, SECOND - 1] {
                assert_eq!(gate.admit(probe("probe:p:a", Some(activity)), 10 * SECOND), Admission::Passive);
            }
            assert_eq!(gate.stats().admitted, 0);
            assert_eq!(gate.stats().tracked_keys, 0);
        }
    }

    #[test]
    fn all_routine_liveness_families_are_observation_only() {
        let mut gate = NotificationAdmission::new(0);
        gate.begin_tick(10 * SECOND);
        for key in ["liveness_monitoring:p:a", "withheld_release_notice:p:a"] {
            let notice = Notification { class: NotificationClass::Liveness, ..probe(key, Some(SECOND)) };
            assert_eq!(gate.admit(notice, 10 * SECOND), Admission::Passive);
        }
        assert_eq!(gate.stats().admitted, 0);
    }

    #[test]
    fn never_observed_agents_do_not_receive_probe_proposals() {
        let mut gate = NotificationAdmission::new(0);
        gate.begin_tick(200 * SECOND);
        for activity in [None, Some(0), Some(-1)] {
            assert_eq!(gate.admit(probe("probe:p:a", activity), 200 * SECOND), Admission::NoActivity);
        }
        assert_eq!(gate.stats().tracked_keys, 0);
        assert_eq!(gate.admit(probe("probe:p:a", Some(SECOND)), 200 * SECOND), Admission::Passive);
    }

    #[test]
    fn active_or_future_dated_agents_are_not_counted_as_silent() {
        let mut gate = NotificationAdmission::new(120 * SECOND);
        gate.begin_tick(100 * SECOND);
        for now in [0, SECOND, 120 * SECOND] {
            assert_eq!(gate.admit(probe("probe:p:a", Some(SECOND)), now), Admission::RecentlyActive);
        }
        assert_eq!(gate.admit(probe("probe:p:a", Some(SECOND)), 121 * SECOND), Admission::Passive);
        assert_eq!(gate.stats().admitted, 0);
    }

    #[test]
    fn passive_population_cannot_consume_conflict_capacity_or_budget() {
        let mut gate = NotificationAdmission::with_limits(0, 1, 1);
        gate.begin_tick(10 * SECOND);
        for agent in 0..10_000 {
            let key = format!("probe:p:Agent{agent}");
            assert_eq!(gate.admit(probe(&key, Some(SECOND)), 10 * SECOND), Admission::Passive);
        }
        assert_eq!(gate.stats().tracked_keys, 0);
        assert_eq!(gate.admit(conflict("deadlock:p:a"), 10 * SECOND), Admission::Admitted);
        // Passive observations remain possible even after the mail budget is spent.
        assert_eq!(gate.admit(probe("probe:p:late", Some(SECOND)), 10 * SECOND), Admission::Passive);
        assert_eq!(gate.stats().admitted, 1);
    }

    #[test]
    fn large_conflict_population_makes_bounded_fair_progress() {
        const POPULATION: usize = 940;
        let mut gate = NotificationAdmission::new(0);
        let mut delivered = HashSet::new();
        for tick in 0..POPULATION.div_ceil(MAX_NOTIFICATIONS_PER_TICK) {
            let now = 1_000_000_000 + i64::try_from(tick).unwrap() * 250_000;
            gate.begin_tick(now);
            let mut batch = 0;
            for agent in 0..POPULATION {
                let key = format!("deadlock:p:Agent{agent:04}");
                if gate.admit(conflict(&key), now) == Admission::Admitted {
                    assert!(delivered.insert(agent));
                    batch += 1;
                }
            }
            assert!(batch <= MAX_NOTIFICATIONS_PER_TICK);
        }
        assert_eq!(delivered.len(), POPULATION);
    }

    #[test]
    fn deferred_conflict_does_not_consume_cooldown() {
        let mut gate = NotificationAdmission::with_limits(0, 1, 10);
        gate.begin_tick(10 * SECOND);
        assert_eq!(gate.admit(conflict("deadlock:p:a"), 10 * SECOND), Admission::Admitted);
        assert_eq!(gate.admit(conflict("deadlock:p:b"), 10 * SECOND), Admission::Deferred);
        gate.begin_tick(11 * SECOND);
        assert_eq!(gate.admit(conflict("deadlock:p:b"), 11 * SECOND), Admission::Admitted);
    }

    #[test]
    fn full_cache_preserves_existing_conflict_cooldowns() {
        let mut gate = NotificationAdmission::with_limits(0, 16, 1);
        gate.begin_tick(10 * SECOND);
        assert_eq!(gate.admit(conflict("deadlock:p:a"), 10 * SECOND), Admission::Admitted);
        assert_eq!(gate.admit(conflict("deadlock:p:b"), 10 * SECOND), Admission::Capacity);
        gate.begin_tick(20 * SECOND);
        assert_eq!(gate.admit(conflict("deadlock:p:a"), 20 * SECOND), Admission::Duplicate);
        assert_eq!(gate.stats().tracked_keys, 1);
    }

    #[test]
    fn expired_conflict_cooldowns_free_capacity() {
        let mut gate = NotificationAdmission::with_limits(0, 16, 1);
        gate.begin_tick(SECOND);
        assert_eq!(gate.admit(conflict("deadlock:p:a"), SECOND), Admission::Admitted);
        gate.begin_tick(300 * SECOND);
        assert_eq!(gate.admit(conflict("deadlock:p:a"), 300 * SECOND), Admission::Duplicate);
        gate.begin_tick(301 * SECOND);
        assert_eq!(gate.admit(conflict("deadlock:p:b"), 301 * SECOND), Admission::Admitted);
    }

    #[test]
    fn conflict_project_scopes_are_independent() {
        let mut gate = NotificationAdmission::new(0);
        gate.begin_tick(10 * SECOND);
        for key in ["deadlock:p1:a", "deadlock:p2:a"] {
            assert_eq!(gate.admit(conflict(key), 10 * SECOND), Admission::Admitted);
        }
        assert_eq!(gate.stats().admitted, 2);
    }

    #[test]
    fn zero_cooldown_still_coalesces_the_current_tick() {
        let mut gate = NotificationAdmission::new(0);
        gate.begin_tick(SECOND);
        let notice = || Notification { cooldown_micros: 0, ..conflict("deadlock:p:a") };
        assert_eq!(gate.admit(notice(), SECOND), Admission::Admitted);
        assert_eq!(gate.admit(notice(), SECOND), Admission::Duplicate);
    }

    #[test]
    fn clock_regression_does_not_rearm_a_cooldown() {
        let mut gate = NotificationAdmission::new(0);
        gate.begin_tick(100 * SECOND);
        assert_eq!(gate.admit(conflict("deadlock:p:a"), 100 * SECOND), Admission::Admitted);
        gate.begin_tick(SECOND);
        assert_eq!(gate.admit(conflict("deadlock:p:a"), SECOND), Admission::Duplicate);
    }
}
