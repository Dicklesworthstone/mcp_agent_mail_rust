//! Independent deadlines and an aggregate work budget for archive repair.
//!
//! A shared wake-up is not evidence that every source needs another scan. In
//! particular, checkpoint contention in one lane must not accelerate unrelated
//! healthy lanes. Storage still owns every cursor, retry budget and publication
//! decision; this module only decides whether to invoke an existing turn.

use std::time::{Duration, Instant};

use super::{REPAIR_CATCH_UP_POLL_INTERVAL, next_poll_interval};

pub(super) const ACTIVE_QUANTUM: Duration = Duration::from_millis(250);

/// A cooperative admission bound, shared by all lanes and their observers.
/// An individual I/O operation can overrun it; its entire elapsed time still
/// earns rest. Four units of rest per unit of active wall time target at most
/// 20% duty for this worker, not a cap on independent database/Git threads.
pub(super) struct MaintenanceTurn {
    started: Instant,
}

impl MaintenanceTurn {
    pub(super) const fn new(started: Instant) -> Self {
        Self { started }
    }

    pub(super) fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }

    pub(super) fn exhausted(&self, now: Instant) -> bool {
        self.elapsed(now) >= ACTIVE_QUANTUM
    }

    pub(super) fn rest(&self, now: Instant) -> Duration {
        self.elapsed(now).saturating_mul(4)
    }
}

/// Scheduling evidence only. Neither a retry deadline nor a runnable scan
/// grants source, checkpoint, or archive publication authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkHint {
    Idle,
    Runnable,
    RetryAt(Instant),
    Contended { progressed: bool },
    ResourcePaused,
    Interrupted,
}

impl WorkHint {
    pub(super) fn after_report(scan_pending: bool, retry_at: Option<Instant>) -> Self {
        if scan_pending {
            Self::Runnable
        } else {
            retry_at.map_or(Self::Idle, Self::RetryAt)
        }
    }

    #[cfg(test)]
    pub(super) const fn needs_follow_up(self) -> bool {
        matches!(
            self,
            Self::Runnable | Self::RetryAt(_) | Self::Contended { .. }
        )
    }
}

// The original deadline controls use booleans for their two possible results.
// Production repair drivers supply the richer evidence above directly.
#[cfg(test)]
impl From<bool> for WorkHint {
    fn from(backlog: bool) -> Self {
        if backlog { Self::Runnable } else { Self::Idle }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lane {
    Messages,
    Agents,
    Reservations,
}

impl Lane {
    const fn index(self) -> usize {
        match self {
            Self::Messages => 0,
            Self::Agents => 1,
            Self::Reservations => 2,
        }
    }
}

struct LaneState {
    enabled: bool,
    completed: Option<Instant>,
    delay: Duration,
    contention_streak: u32,
}

impl LaneState {
    const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            completed: None,
            delay: Duration::ZERO,
            contention_streak: 0,
        }
    }

    fn remaining(&self, now: Instant) -> Duration {
        self.completed.map_or(Duration::ZERO, |completed| {
            self.delay
                .saturating_sub(now.saturating_duration_since(completed))
        })
    }

    fn complete(&mut self, now: Instant, idle: Duration, hint: WorkHint) {
        self.completed = Some(now);
        self.delay = match hint {
            WorkHint::Runnable => next_poll_interval(idle, true),
            WorkHint::RetryAt(at) => at.saturating_duration_since(now).min(idle),
            WorkHint::Contended { progressed } => {
                if progressed {
                    self.contention_streak = 0;
                }
                let delay = Duration::from_secs(1_u64 << self.contention_streak.min(6));
                self.contention_streak = self.contention_streak.saturating_add(1);
                delay.min(idle)
            }
            WorkHint::Idle | WorkHint::ResourcePaused | WorkHint::Interrupted => idle,
        }
        .max(REPAIR_CATCH_UP_POLL_INTERVAL);
        if !matches!(hint, WorkHint::Contended { .. }) {
            self.contention_streak = 0;
        }
    }
}

pub(super) struct RepairSchedule {
    idle_interval: Duration,
    lanes: [LaneState; 3],
    next_lane: usize,
}

impl RepairSchedule {
    pub(super) fn new(
        idle_interval: Duration,
        messages_enabled: bool,
        agents_enabled: bool,
        reservations_enabled: bool,
    ) -> Self {
        Self {
            // Production supplies at least sixty seconds. Keep even malformed
            // or test-supplied intervals from making the outer worker spin.
            idle_interval: idle_interval.max(REPAIR_CATCH_UP_POLL_INTERVAL),
            lanes: [
                LaneState::new(messages_enabled),
                LaneState::new(agents_enabled),
                LaneState::new(reservations_enabled),
            ],
            next_lane: 0,
        }
    }

    /// The lane after the last admitted one gets the first opportunity next.
    /// A skipped deadline or spent quantum never consumes its opportunity.
    pub(super) fn lane_order(&self) -> [Lane; 3] {
        const LANES: [Lane; 3] = [Lane::Messages, Lane::Agents, Lane::Reservations];
        std::array::from_fn(|offset| LANES[(self.next_lane + offset) % LANES.len()])
    }

    pub(super) fn run_in_turn_if_due<E, R: Copy + Into<WorkHint>>(
        &mut self,
        turn: &MaintenanceTurn,
        lane: Lane,
        mut clock: impl FnMut() -> Instant,
        work: impl FnOnce() -> Option<Result<R, E>>,
    ) -> Option<Result<R, E>> {
        if turn.exhausted(clock()) {
            return None;
        }
        self.run_if_due(lane, clock, work)
    }

    /// Gate BEFORE any source work. A skipped lane neither runs `work` nor
    /// moves its deadline; repeated peer wakes cannot starve its next scan.
    ///
    /// `None` from `work` means no source was available, not a successful empty
    /// scan. It and source errors use ordinary backoff without manufacturing a
    /// storage report. Runnable scans and bounded contention retries may use
    /// the short cadence; cooling debt retains its own deadline.
    /// Measure the next delay from completion, including slow source/observer
    /// work, so a turn that overruns its interval still gets a real pause.
    pub(super) fn run_if_due<E, R: Copy + Into<WorkHint>>(
        &mut self,
        lane: Lane,
        mut clock: impl FnMut() -> Instant,
        work: impl FnOnce() -> Option<Result<R, E>>,
    ) -> Option<Result<R, E>> {
        let state = &mut self.lanes[lane.index()];
        if !state.enabled || !state.remaining(clock()).is_zero() {
            return None;
        }
        let result = work();
        let hint = match result.as_ref() {
            Some(Ok(hint)) => (*hint).into(),
            Some(Err(_)) | None => WorkHint::Idle,
        };
        state.complete(clock(), self.idle_interval, hint);
        self.next_lane = (lane.index() + 1) % self.lanes.len();
        result
    }

    /// Earliest enabled deadline, never an immediate re-poll. A different lane
    /// can become due while a slow turn or report runs; the floor yields the
    /// worker without losing that deadline. Disabled lanes create no wakes.
    pub(super) fn next_delay(&self, now: Instant) -> Duration {
        self.lanes
            .iter()
            .filter(|state| state.enabled)
            .map(|state| state.remaining(now))
            .min()
            .unwrap_or(self.idle_interval)
            .max(REPAIR_CATCH_UP_POLL_INTERVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const LANES: [Lane; 3] = [Lane::Messages, Lane::Agents, Lane::Reservations];
    const IDLE: Duration = Duration::from_mins(1);

    #[test]
    fn gh354_one_busy_lane_does_not_accelerate_healthy_peers() {
        for busy in LANES {
            let mut schedule = RepairSchedule::new(IDLE, true, true, true);
            let start = Instant::now();
            let mut calls = [0; 3];
            for second in 0..60 {
                let now = start + Duration::from_secs(second);
                for lane in LANES {
                    let result = schedule.run_if_due(
                        lane,
                        || now,
                        || {
                            calls[lane.index()] += 1;
                            Some(Ok::<_, ()>(lane == busy))
                        },
                    );
                    let due = second == 0 || lane == busy;
                    assert_eq!(result, due.then_some(Ok(lane == busy)));
                }
                assert_eq!(schedule.next_delay(now), Duration::from_secs(1));
            }
            for lane in LANES {
                assert_eq!(calls[lane.index()], if lane == busy { 60 } else { 1 });
                // A healthy lane still scans at its original deadline even
                // after fifty-nine skipped peer wakes. It is not disabled.
                assert_eq!(
                    schedule.run_if_due(lane, || start + IDLE, || Some(Ok::<_, ()>(false))),
                    Some(Ok(false))
                );
            }
            assert_eq!(schedule.next_delay(start + IDLE), IDLE);
        }
    }

    #[test]
    fn gh354_short_retry_returns_to_idle_after_backlog_drains() {
        let start = Instant::now();
        let mut schedule = RepairSchedule::new(IDLE, false, false, true);
        assert_eq!(
            schedule.run_if_due(Lane::Reservations, || start, || Some(Ok::<_, ()>(true))),
            Some(Ok(true))
        );
        let early = start + Duration::from_millis(999);
        assert_eq!(
            schedule.run_if_due::<(), bool>(Lane::Reservations, || early, || panic!("early SQL")),
            None
        );
        let due = start + Duration::from_secs(1);
        assert_eq!(
            schedule.run_if_due(Lane::Reservations, || due, || Some(Ok::<_, ()>(false))),
            Some(Ok(false))
        );
        assert_eq!(schedule.next_delay(due), IDLE);
        assert_eq!(
            schedule.run_if_due::<(), bool>(
                Lane::Reservations,
                || due + IDLE / 2,
                || panic!("completed backlog still on short cadence"),
            ),
            None
        );
        assert_eq!(
            schedule.run_if_due(
                Lane::Reservations,
                || due + IDLE,
                || Some(Ok::<_, ()>(true))
            ),
            Some(Ok(true))
        );
        assert_eq!(schedule.next_delay(due + IDLE), Duration::from_secs(1));
    }

    #[test]
    fn gh354_disabled_lanes_do_not_run_or_schedule_wakes() {
        let start = Instant::now();
        for messages in [false, true] {
            for agents in [false, true] {
                for reservations in [false, true] {
                    let enabled = [messages, agents, reservations];
                    let mut schedule = RepairSchedule::new(IDLE, messages, agents, reservations);
                    let mut calls = [0; 3];
                    for second in 0..=60 {
                        let now = start + Duration::from_secs(second);
                        for lane in LANES {
                            let result = schedule.run_if_due(
                                lane,
                                || now,
                                || {
                                    assert!(
                                        enabled[lane.index()],
                                        "disabled lane touched its source"
                                    );
                                    calls[lane.index()] += 1;
                                    Some(Ok::<_, ()>(false))
                                },
                            );
                            let due = enabled[lane.index()] && second % 60 == 0;
                            assert_eq!(result, due.then_some(Ok(false)), "{enabled:?}: {lane:?}");
                        }
                        let remaining = if enabled.contains(&true) {
                            IDLE.checked_sub(Duration::from_secs(second % 60)).expect(
                                "the second within a minute is shorter than the idle interval",
                            )
                        } else {
                            IDLE
                        };
                        assert_eq!(schedule.next_delay(now), remaining, "{enabled:?}");
                    }
                    for lane in LANES {
                        assert_eq!(
                            calls[lane.index()],
                            if enabled[lane.index()] { 2 } else { 0 }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn gh354_errors_and_unavailable_sources_keep_real_backoff() {
        let start = Instant::now();
        let mut schedule = RepairSchedule::new(IDLE, true, true, true);
        assert_eq!(
            schedule.run_if_due(
                Lane::Messages,
                || start,
                || Some(Err::<bool, _>("source replaced"))
            ),
            Some(Err("source replaced"))
        );
        assert_eq!(
            schedule.run_if_due::<(), bool>(Lane::Agents, || start, || None),
            None
        );
        assert_eq!(
            schedule.run_if_due::<(), bool>(Lane::Reservations, || start, || None),
            None
        );
        assert_eq!(schedule.next_delay(start), IDLE);
        for second in 1..60 {
            for lane in LANES {
                assert_eq!(
                    schedule.run_if_due::<(), bool>(
                        lane,
                        || start + Duration::from_secs(second),
                        || panic!("source unavailability was treated as backlog"),
                    ),
                    None
                );
            }
        }
        for lane in LANES {
            assert_eq!(
                schedule.run_if_due(lane, || start + IDLE, || Some(Ok::<_, ()>(false))),
                Some(Ok(false))
            );
        }
    }

    #[test]
    fn gh354_slow_work_is_followed_by_a_full_lane_pause() {
        for backlog in [false, true] {
            let start = Instant::now();
            let clock = Cell::new(start);
            let mut schedule = RepairSchedule::new(IDLE, false, false, true);
            assert_eq!(
                schedule.run_if_due(
                    Lane::Reservations,
                    || clock.get(),
                    || {
                        clock.set(start + Duration::from_mins(2));
                        Some(Ok::<_, ()>(backlog))
                    },
                ),
                Some(Ok(backlog))
            );
            let delay = next_poll_interval(IDLE, backlog);
            assert_eq!(schedule.next_delay(clock.get()), delay);
            assert_eq!(
                schedule.run_if_due::<(), bool>(
                    Lane::Reservations,
                    || clock.get(),
                    || panic!("slow work immediately rearmed itself"),
                ),
                None
            );
        }
    }

    #[test]
    fn gh354_slow_peer_does_not_postpone_an_already_due_lane() {
        let start = Instant::now();
        let mut schedule = RepairSchedule::new(IDLE, true, true, true);
        for lane in LANES {
            assert_eq!(
                schedule.run_if_due(
                    lane,
                    || start,
                    || Some(Ok::<_, ()>(lane == Lane::Reservations))
                ),
                Some(Ok(lane == Lane::Reservations))
            );
        }
        let clock = Cell::new(start + Duration::from_secs(1));
        assert_eq!(
            schedule.run_if_due(
                Lane::Reservations,
                || clock.get(),
                || {
                    clock.set(start + Duration::from_secs(61));
                    Some(Ok::<_, ()>(false))
                },
            ),
            Some(Ok(false))
        );
        assert_eq!(schedule.next_delay(clock.get()), Duration::from_secs(1));
        let now = clock.get() + Duration::from_secs(1);
        for lane in [Lane::Messages, Lane::Agents] {
            assert_eq!(
                schedule.run_if_due(lane, || now, || Some(Ok::<_, ()>(false))),
                Some(Ok(false))
            );
        }
        assert_eq!(schedule.next_delay(now), Duration::from_secs(59));
    }

    #[test]
    fn gh354_zero_and_extreme_intervals_never_spin_or_overflow() {
        let start = Instant::now();
        for interval in [Duration::ZERO, Duration::MAX] {
            let mut schedule = RepairSchedule::new(interval, false, false, true);
            assert_eq!(schedule.next_delay(start), Duration::from_secs(1));
            assert_eq!(
                schedule.run_if_due(Lane::Reservations, || start, || Some(Ok::<_, ()>(false))),
                Some(Ok(false))
            );
            assert_eq!(
                schedule.next_delay(start),
                interval.max(Duration::from_secs(1))
            );
        }
    }

    #[test]
    fn gh354_clock_regression_cannot_force_an_early_scan() {
        let start = Instant::now();
        let mut schedule = RepairSchedule::new(IDLE, false, false, true);
        let completed = start + Duration::from_secs(10);
        assert_eq!(
            schedule.run_if_due(
                Lane::Reservations,
                || completed,
                || Some(Ok::<_, ()>(false))
            ),
            Some(Ok(false))
        );
        assert_eq!(schedule.next_delay(start), IDLE);
        assert_eq!(
            schedule.run_if_due::<(), bool>(
                Lane::Reservations,
                || start,
                || panic!("clock regression")
            ),
            None
        );
    }

    #[test]
    fn gh383_busy_lanes_share_one_quantum_and_twenty_percent_duty() {
        for costs in [
            [Duration::from_millis(80); 3],
            [Duration::from_millis(300); 3],
            [
                Duration::from_millis(1_800),
                Duration::from_millis(70),
                Duration::from_millis(40),
            ],
        ] {
            let started = Instant::now();
            let clock = Cell::new(started);
            let mut schedule = RepairSchedule::new(IDLE, true, true, true);
            let mut calls = [0_usize; 3];
            let mut total_active = Duration::ZERO;
            for _ in 0..60 {
                let turn = MaintenanceTurn::new(clock.get());
                let mut admitted = 0;
                for lane in schedule.lane_order() {
                    let result = schedule.run_in_turn_if_due(
                        &turn,
                        lane,
                        || clock.get(),
                        || {
                            assert!(turn.elapsed(clock.get()) < ACTIVE_QUANTUM);
                            admitted += 1;
                            calls[lane.index()] += 1;
                            clock.set(clock.get() + costs[lane.index()]);
                            Some(Ok::<_, ()>(WorkHint::Runnable))
                        },
                    );
                    if result.is_some() && turn.exhausted(clock.get()) {
                        assert_eq!(schedule.lane_order()[0].index(), (lane.index() + 1) % 3);
                    }
                }
                assert!(admitted > 0, "a runnable lane was starved");
                let active = turn.elapsed(clock.get());
                total_active += active;
                let rest = turn.rest(clock.get()).max(schedule.next_delay(clock.get()));
                assert!(rest >= active * 4, "an overrun lost its compensating rest");
                clock.set(clock.get() + rest);
            }
            assert!(calls.iter().all(|count| *count >= 20), "{calls:?}");
            assert!(total_active * 5 <= clock.get().duration_since(started));
        }
    }

    #[test]
    fn gh383_pool_and_observer_work_spend_the_same_quantum() {
        let started = Instant::now();
        let turn = MaintenanceTurn::new(started);
        let clock = Cell::new(started + Duration::from_millis(180));
        let mut schedule = RepairSchedule::new(IDLE, true, true, true);
        assert_eq!(
            schedule.run_in_turn_if_due(
                &turn,
                Lane::Messages,
                || clock.get(),
                || {
                    clock.set(clock.get() + Duration::from_millis(20)); // source work
                    clock.set(clock.get() + Duration::from_millis(80)); // actual observer work
                    Some(Ok::<_, ()>(WorkHint::Runnable))
                }
            ),
            Some(Ok(WorkHint::Runnable))
        );
        for lane in [Lane::Agents, Lane::Reservations] {
            assert_eq!(
                schedule.run_in_turn_if_due::<(), WorkHint>(
                    &turn,
                    lane,
                    || clock.get(),
                    || { panic!("observer overrun must prevent another admitted lane") }
                ),
                None
            );
        }
        assert_eq!(schedule.lane_order()[0], Lane::Agents);
        assert_eq!(turn.rest(clock.get()), Duration::from_millis(1_120));
    }

    #[test]
    fn gh383_cooling_debt_uses_its_deadline_and_keeps_discovery_bounded() {
        let start = Instant::now();
        let mut schedule = RepairSchedule::new(IDLE, true, false, false);
        let ready = start + Duration::from_secs(37);
        schedule.run_if_due(
            Lane::Messages,
            || start,
            || Some(Ok::<_, ()>(WorkHint::RetryAt(ready))),
        );
        assert_eq!(schedule.next_delay(start), Duration::from_secs(37));
        for second in 1..37 {
            assert_eq!(
                schedule.run_if_due::<(), WorkHint>(
                    Lane::Messages,
                    || start + Duration::from_secs(second),
                    || panic!("cooling-only debt must not read or rewrite a checkpoint")
                ),
                None
            );
        }
        assert_eq!(
            schedule.run_if_due(
                Lane::Messages,
                || ready,
                || Some(Ok::<_, ()>(WorkHint::RetryAt(
                    ready + Duration::from_secs(600)
                )))
            ),
            Some(Ok(WorkHint::RetryAt(ready + Duration::from_secs(600))))
        );
        assert_eq!(schedule.next_delay(ready), IDLE);
        // New tail work is discovered on the ordinary cadence even while a
        // retained retry's deadline is much later. Its own cooldown is intact.
        assert_eq!(
            schedule.run_if_due(
                Lane::Messages,
                || ready + IDLE,
                || Some(Ok::<_, ()>(WorkHint::after_report(
                    true,
                    Some(ready + Duration::from_secs(600))
                )))
            ),
            Some(Ok(WorkHint::Runnable))
        );
        assert_eq!(
            schedule.next_delay(ready + IDLE),
            REPAIR_CATCH_UP_POLL_INTERVAL
        );
    }

    #[test]
    fn gh383_contention_backs_off_without_delaying_other_lanes() {
        let start = Instant::now();
        let mut schedule = RepairSchedule::new(IDLE, true, true, true);
        let mut calls = [0; 3];
        for second in 0..120 {
            let now = start + Duration::from_secs(second);
            for lane in LANES {
                schedule.run_if_due(
                    lane,
                    || now,
                    || {
                        calls[lane.index()] += 1;
                        Some(Ok::<_, ()>(match lane {
                            Lane::Messages => WorkHint::Contended { progressed: false },
                            Lane::Agents => WorkHint::Runnable,
                            Lane::Reservations => WorkHint::Idle,
                        }))
                    },
                );
            }
        }
        assert_eq!(calls, [7, 120, 2]);
        let now = start + Duration::from_secs(123);
        assert_eq!(
            schedule.run_if_due(
                Lane::Messages,
                || now,
                || Some(Ok::<_, ()>(WorkHint::Contended { progressed: true }))
            ),
            Some(Ok(WorkHint::Contended { progressed: true }))
        );
        assert_eq!(
            schedule.run_if_due(
                Lane::Messages,
                || now + Duration::from_secs(1),
                || Some(Ok::<_, ()>(WorkHint::Idle))
            ),
            Some(Ok(WorkHint::Idle))
        );
    }
}
