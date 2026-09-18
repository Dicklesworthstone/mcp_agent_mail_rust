//! Bounded admission for best-effort ATC notifications, before persistence.
//!
//! A new decision ID is not a new reason to contact an agent. Liveness prompts
//! are admitted once per family and observed-activity epoch; recurring conflict
//! notices are admitted once per semantic cooldown. Neither a timer tick nor
//! another copy of a population snapshot rearms an unanswered prompt.
//!
//! This gate records admission, not successful delivery. Prompts are best-effort
//! and are not retried within the same inactivity episode. It must never gate
//! reservation mutations or notices reporting their outcome. No entry here is
//! an acknowledgement, an activity observation, or evidence of agent death.

use std::collections::HashMap;

/// Keep routine notifications well below the operator's 64-action drain budget.
/// The engine independently limits liveness reviews (including releases) to 8.
pub(crate) const MAX_NOTIFICATIONS_PER_TICK: usize = 16;
/// Three liveness families for a 4,096-agent population, plus conflict headroom.
/// At capacity, fail closed for optional notifications rather than evicting an
/// unanswered prompt and immediately sending it again. Critical effects bypass
/// this cache entirely. Expired conflict cooldowns are reclaimed each tick.
const MAX_NOTIFICATION_KEYS: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationClass {
    Probe,
    Liveness,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    Admitted,
    Duplicate,
    NoActivity,
    RecentlyActive,
    Deferred,
    Capacity,
}

/// Process-local counters. Suppression is deliberately not another DB event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AtcDeliveryStats {
    pub admitted: u64,
    pub duplicate: u64,
    pub no_activity: u64,
    pub recently_active: u64,
    pub deferred: u64,
    pub capacity_suppressed: u64,
    pub tracked_keys: usize,
}

#[derive(Debug, Clone, Copy)]
enum Entry {
    Activity(i64),
    Cooldown(i64),
}

pub(crate) struct Notification<'a> {
    /// The planner's project-, agent-, and family-scoped semantic cooldown key.
    pub key: &'a str,
    pub class: NotificationClass,
    pub last_activity_micros: Option<i64>,
    pub cooldown_micros: i64,
}

#[derive(Debug)]
pub(crate) struct NotificationAdmission {
    entries: HashMap<String, Entry>,
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
            entries: HashMap::new(),
            limit,
            key_limit,
            remaining: 0,
            min_probe_silence_micros: min_probe_silence_micros.max(0),
            stats: AtcDeliveryStats::default(),
        }
    }

    pub(crate) fn begin_tick(&mut self, now_micros: i64) {
        self.remaining = self.limit;
        // Never age out unanswered liveness entries: doing so rearms silent
        // agents without new evidence. Conflict entries can safely expire.
        self.entries.retain(|_, entry| match entry {
            Entry::Activity(_) => true,
            Entry::Cooldown(until) => *until > now_micros,
        });
    }

    pub(crate) fn admit(&mut self, notification: Notification<'_>, now_micros: i64) -> Admission {
        let next_entry = match notification.class {
            NotificationClass::Probe | NotificationClass::Liveness => {
                let Some(activity) = notification.last_activity_micros.filter(|ts| *ts > 0) else {
                    return self.record(Admission::NoActivity);
                };
                if notification.class == NotificationClass::Probe
                    && (activity >= now_micros
                        || now_micros.saturating_sub(activity) < self.min_probe_silence_micros)
                {
                    return self.record(Admission::RecentlyActive);
                }
                if let Some(Entry::Activity(previous)) = self.entries.get(notification.key)
                    && activity <= *previous
                {
                    return self.record(Admission::Duplicate);
                }
                Entry::Activity(activity)
            }
            NotificationClass::Conflict => {
                if let Some(Entry::Cooldown(until)) = self.entries.get(notification.key)
                    && now_micros < *until
                {
                    return self.record(Admission::Duplicate);
                }
                // A zero/negative cooldown must still coalesce duplicates in
                // the same tick, rather than filling the entire action budget.
                Entry::Cooldown(now_micros.saturating_add(notification.cooldown_micros.max(1)))
            }
        };

        if self.remaining == 0 {
            // Do not consume an activity epoch or cooldown. The planner can
            // propose the still-relevant notification again on a later tick.
            return self.record(Admission::Deferred);
        }
        if !self.entries.contains_key(notification.key) && self.entries.len() >= self.key_limit {
            return self.record(Admission::Capacity);
        }

        self.entries.insert(notification.key.to_owned(), next_entry);
        self.remaining -= 1;
        self.record(Admission::Admitted)
    }

    fn record(&mut self, admission: Admission) -> Admission {
        let count = match admission {
            Admission::Admitted => &mut self.stats.admitted,
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
        AtcDeliveryStats {
            tracked_keys: self.entries.len(),
            ..self.stats
        }
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
    fn ten_thousand_ticks_do_not_rearm_an_unanswered_probe() {
        let mut gate = NotificationAdmission::new(120 * SECOND);
        for tick in 0..10_000 {
            let now = (200 + tick * 120) * SECOND;
            gate.begin_tick(now);
            let result = gate.admit(probe("probe:project:BlueFox", Some(SECOND)), now);
            assert_eq!(result, if tick == 0 { Admission::Admitted } else { Admission::Duplicate });
        }
        assert_eq!(gate.stats().admitted, 1);
        assert_eq!(gate.stats().duplicate, 9_999);
        assert_eq!(gate.stats().tracked_keys, 1);
    }

    #[test]
    fn only_strictly_new_activity_rearms_a_prompt() {
        let mut gate = NotificationAdmission::new(0);
        gate.begin_tick(10 * SECOND);
        assert_eq!(gate.admit(probe("probe:p:a", Some(SECOND)), 10 * SECOND), Admission::Admitted);
        for activity in [SECOND, SECOND - 1, 1] {
            assert_eq!(gate.admit(probe("probe:p:a", Some(activity)), 10 * SECOND), Admission::Duplicate);
        }
        assert_eq!(gate.admit(probe("probe:p:a", Some(2 * SECOND)), 10 * SECOND), Admission::Admitted);
        assert_eq!(gate.stats().tracked_keys, 1, "new epochs replace, not accumulate, entries");
    }

    #[test]
    fn never_observed_agents_do_not_receive_durable_probe_proposals() {
        let mut gate = NotificationAdmission::new(0);
        gate.begin_tick(200 * SECOND);
        for activity in [None, Some(0), Some(-1)] {
            assert_eq!(gate.admit(probe("probe:p:a", activity), 200 * SECOND), Admission::NoActivity);
        }
        assert_eq!(gate.stats().tracked_keys, 0);
        assert_eq!(gate.admit(probe("probe:p:a", Some(SECOND)), 200 * SECOND), Admission::Admitted);
    }

    #[test]
    fn active_or_future_dated_agents_do_not_consume_their_next_probe() {
        let mut gate = NotificationAdmission::new(120 * SECOND);
        gate.begin_tick(100 * SECOND);
        for now in [0, SECOND, 120 * SECOND] {
            assert_eq!(gate.admit(probe("probe:p:a", Some(SECOND)), now), Admission::RecentlyActive);
        }
        assert_eq!(gate.stats().tracked_keys, 0);
        assert_eq!(gate.admit(probe("probe:p:a", Some(SECOND)), 121 * SECOND), Admission::Admitted);
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
                let key = format!("deadlock:project:Agent{agent:04}");
                if gate.admit(conflict(&key), now) == Admission::Admitted {
                    assert!(delivered.insert(agent), "duplicate proposal escaped admission");
                    batch += 1;
                }
            }
            assert!(batch <= MAX_NOTIFICATIONS_PER_TICK);
        }
        assert_eq!(delivered.len(), POPULATION);
        assert_eq!(gate.stats().admitted, 940);
    }

    #[test]
    fn deferred_notification_does_not_consume_epoch_or_cooldown() {
        let mut gate = NotificationAdmission::with_limits(0, 1, 10);
        gate.begin_tick(10 * SECOND);
        assert_eq!(gate.admit(conflict("deadlock:p:a"), 10 * SECOND), Admission::Admitted);
        assert_eq!(gate.admit(probe("probe:p:b", Some(SECOND)), 10 * SECOND), Admission::Deferred);
        gate.begin_tick(11 * SECOND);
        assert_eq!(gate.admit(probe("probe:p:b", Some(SECOND)), 11 * SECOND), Admission::Admitted);
    }

    #[test]
    fn full_cache_does_not_evict_and_rearm_unanswered_agents() {
        let mut gate = NotificationAdmission::with_limits(0, 16, 1);
        gate.begin_tick(10 * SECOND);
        assert_eq!(gate.admit(probe("probe:p:a", Some(SECOND)), 10 * SECOND), Admission::Admitted);
        assert_eq!(gate.admit(probe("probe:p:b", Some(SECOND)), 10 * SECOND), Admission::Capacity);
        gate.begin_tick(20 * SECOND);
        assert_eq!(gate.admit(probe("probe:p:a", Some(SECOND)), 20 * SECOND), Admission::Duplicate);
        assert_eq!(gate.admit(probe("probe:p:a", Some(2 * SECOND)), 20 * SECOND), Admission::Admitted);
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
        assert_eq!(gate.stats().tracked_keys, 1);
    }

    #[test]
    fn project_and_family_scopes_are_independent() {
        let mut gate = NotificationAdmission::new(0);
        gate.begin_tick(10 * SECOND);
        for key in ["probe:p1:a", "probe:p2:a", "monitoring:p1:a"] {
            assert_eq!(gate.admit(probe(key, Some(SECOND)), 10 * SECOND), Admission::Admitted);
        }
        assert_eq!(gate.stats().admitted, 3);
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
