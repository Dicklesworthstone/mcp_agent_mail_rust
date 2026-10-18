//! Independent deadlines for the archive worker's three repair lanes (GH#354).
//!
//! A shared wake-up is not evidence that every source needs another scan. In
//! particular, checkpoint contention in one lane must not accelerate unrelated
//! healthy lanes. Storage still owns every cursor, retry budget and publication
//! decision; this module only decides whether to invoke an existing turn.

use std::time::{Duration, Instant};

use super::{REPAIR_CATCH_UP_POLL_INTERVAL, next_poll_interval};

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
    backlog: bool,
}

impl LaneState {
    const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            completed: None,
            backlog: false,
        }
    }

    fn remaining(&self, now: Instant, idle_interval: Duration) -> Duration {
        self.completed.map_or(Duration::ZERO, |completed| {
            next_poll_interval(idle_interval, self.backlog)
                .saturating_sub(now.saturating_duration_since(completed))
        })
    }
}

pub(super) struct RepairSchedule {
    idle_interval: Duration,
    lanes: [LaneState; 3],
}

impl RepairSchedule {
    pub(super) fn new(
        idle_interval: Duration,
        messages_enabled: bool,
        reservations_enabled: bool,
    ) -> Self {
        Self {
            // Production supplies at least sixty seconds. Keep even malformed
            // or test-supplied intervals from making the outer worker spin.
            idle_interval: idle_interval.max(REPAIR_CATCH_UP_POLL_INTERVAL),
            lanes: [
                LaneState::new(messages_enabled),
                LaneState::new(messages_enabled),
                LaneState::new(reservations_enabled),
            ],
        }
    }

    /// Gate BEFORE any source work. A skipped lane neither runs `work` nor
    /// moves its deadline; repeated peer wakes cannot starve its next scan.
    ///
    /// `None` from `work` means no source was available, not a successful empty
    /// scan. It and source errors use ordinary backoff without manufacturing a
    /// storage report. Only an explicit backlog hint authorizes short retries.
    /// Measure the next delay from completion, including slow source/observer
    /// work, so a turn that overruns its interval still gets a real pause.
    pub(super) fn run_if_due<E>(
        &mut self,
        lane: Lane,
        mut clock: impl FnMut() -> Instant,
        work: impl FnOnce() -> Option<Result<bool, E>>,
    ) -> Option<Result<bool, E>> {
        let state = &mut self.lanes[lane.index()];
        if !state.enabled || !state.remaining(clock(), self.idle_interval).is_zero() {
            return None;
        }
        let result = work();
        state.completed = Some(clock());
        state.backlog = matches!(result.as_ref(), Some(Ok(true)));
        result
    }

    /// Earliest enabled deadline, never an immediate re-poll. A different lane
    /// can become due while a slow turn or report runs; the floor yields the
    /// worker without losing that deadline. Disabled lanes create no wakes.
    pub(super) fn next_delay(&self, now: Instant) -> Duration {
        self.lanes
            .iter()
            .filter(|state| state.enabled)
            .map(|state| state.remaining(now, self.idle_interval))
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
            let mut schedule = RepairSchedule::new(IDLE, true, true);
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
        let mut schedule = RepairSchedule::new(IDLE, false, true);
        assert_eq!(
            schedule.run_if_due(Lane::Reservations, || start, || Some(Ok::<_, ()>(true))),
            Some(Ok(true))
        );
        let early = start + Duration::from_millis(999);
        assert_eq!(
            schedule.run_if_due::<()>(Lane::Reservations, || early, || panic!("early SQL")),
            None
        );
        let due = start + Duration::from_secs(1);
        assert_eq!(
            schedule.run_if_due(Lane::Reservations, || due, || Some(Ok::<_, ()>(false))),
            Some(Ok(false))
        );
        assert_eq!(schedule.next_delay(due), IDLE);
        assert_eq!(
            schedule.run_if_due::<()>(
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
        for (messages, reservations) in [(false, false), (false, true), (true, false)] {
            let mut schedule = RepairSchedule::new(IDLE, messages, reservations);
            for lane in LANES {
                let enabled = if lane == Lane::Reservations {
                    reservations
                } else {
                    messages
                };
                let result = schedule.run_if_due(
                    lane,
                    || start,
                    || {
                        assert!(enabled, "disabled lane touched its source");
                        Some(Ok::<_, ()>(false))
                    },
                );
                assert_eq!(result, enabled.then_some(Ok(false)));
            }
            assert_eq!(schedule.next_delay(start), IDLE);
        }
    }

    #[test]
    fn gh354_errors_and_unavailable_sources_keep_real_backoff() {
        let start = Instant::now();
        let mut schedule = RepairSchedule::new(IDLE, true, true);
        assert_eq!(
            schedule.run_if_due(Lane::Messages, || start, || Some(Err("source replaced"))),
            Some(Err("source replaced"))
        );
        assert_eq!(
            schedule.run_if_due::<()>(Lane::Agents, || start, || None),
            None
        );
        assert_eq!(
            schedule.run_if_due::<()>(Lane::Reservations, || start, || None),
            None
        );
        assert_eq!(schedule.next_delay(start), IDLE);
        for second in 1..60 {
            for lane in LANES {
                assert_eq!(
                    schedule.run_if_due::<()>(
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
            let mut schedule = RepairSchedule::new(IDLE, false, true);
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
                schedule.run_if_due::<()>(
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
        let mut schedule = RepairSchedule::new(IDLE, true, true);
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
            let mut schedule = RepairSchedule::new(interval, false, true);
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
        let mut schedule = RepairSchedule::new(IDLE, false, true);
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
            schedule.run_if_due::<()>(Lane::Reservations, || start, || panic!("clock regression")),
            None
        );
    }
}
