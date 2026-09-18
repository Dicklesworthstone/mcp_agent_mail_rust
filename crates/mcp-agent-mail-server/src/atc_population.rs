//! Incremental population hydration for the public ATC operator boundary.
//!
//! The database query remains a single bounded projection. Applying that
//! projection to the inference engine is separate work: each registration can
//! update schedules and cohort state. Never apply thousands of rows in one tick.
//! While a refresh is incomplete, publish a partial snapshot and defer inference
//! instead of releasing reservations against an only-partially-refreshed roster.

use super::engine::{self, AtcPopulationSyncStats, AtcSummarySnapshot, AtcTickReport};
use mcp_agent_mail_db::models::AtcPopulationAgentRow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};

/// A count bound as well as a cooperative elapsed-time bound. The latter cannot
/// preempt an individual engine update or database operation.
const MAX_AGENTS_PER_SLICE: usize = 32;
const HYDRATION_SLICE: Duration = Duration::from_millis(2);

type AgentKey = (i64, String);

/// Process-local hydration progress, not another durable telemetry stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AtcPopulationHydrationStats {
    pub snapshot_agents: usize,
    pub pending_agents: usize,
    pub applied_agents: usize,
    pub unchanged_agents: usize,
    pub last_slice_agents: usize,
    pub deferred_refreshes: u64,
    pub snapshot_started_at_micros: i64,
}

#[derive(Default)]
struct HydrationState {
    pending: VecDeque<AtcPopulationAgentRow>,
    // Bounded by the most recent query's population limit, not lifetime names.
    // A new refresh cannot replace an unfinished one, so this represents fully
    // applied rows whenever it is consulted for unchanged-row suppression.
    previous_snapshot: HashMap<AgentKey, AtcPopulationAgentRow>,
    snapshot_stats: AtcPopulationSyncStats,
    progress: AtcPopulationHydrationStats,
    resume_pending: bool,
}

impl HydrationState {
    fn refresh_with(
        &mut self,
        now_micros: i64,
        load: impl FnOnce() -> Result<Vec<AtcPopulationAgentRow>, String>,
    ) -> Result<AtcPopulationSyncStats, String> {
        if !self.pending.is_empty() {
            // Do not repeatedly replace the queue with its first page. This
            // also avoids re-querying while an earlier refresh needs CPU time.
            self.progress.deferred_refreshes = self.progress.deferred_refreshes.saturating_add(1);
            return Ok(self.snapshot_stats);
        }
        let rows = load()?;
        let mut projects = HashSet::new();
        let mut stats = AtcPopulationSyncStats::default();
        let mut next_snapshot = HashMap::with_capacity(rows.len());
        let mut pending = VecDeque::new();
        let mut unchanged = 0;
        for row in rows {
            projects.insert(row.project_id);
            stats.agents += 1;
            stats.active_agents += usize::from(row.last_active_ts > 0);
            let key = (row.project_id, row.name.clone());
            if self.previous_snapshot.get(&key) == Some(&row) {
                unchanged += 1;
            } else {
                pending.push_back(row.clone());
            }
            next_snapshot.insert(key, row);
        }
        stats.projects = projects.len();
        self.previous_snapshot = next_snapshot;
        self.pending = pending;
        self.snapshot_stats = stats;
        self.progress = AtcPopulationHydrationStats {
            snapshot_agents: stats.agents,
            pending_agents: self.pending.len(),
            unchanged_agents: unchanged,
            deferred_refreshes: self.progress.deferred_refreshes,
            snapshot_started_at_micros: now_micros,
            ..AtcPopulationHydrationStats::default()
        };
        self.resume_pending = !self.pending.is_empty();
        Ok(stats)
    }

    fn drain_with(
        &mut self,
        mut apply: impl FnMut(&AtcPopulationAgentRow),
        mut has_time: impl FnMut() -> bool,
    ) -> usize {
        let mut applied = 0;
        self.progress.last_slice_agents = 0;
        while applied < MAX_AGENTS_PER_SLICE && (applied == 0 || has_time()) {
            let Some(row) = self.pending.front() else {
                break;
            };
            // Retain the head until the update returns: a panic must not lose
            // the row. Reapplying an interrupted snapshot is monotonic in the
            // engine, and poisoned locks are recovered by the caller.
            apply(row);
            let _ = self.pending.pop_front();
            applied += 1;
            self.progress.applied_agents += 1;
            self.progress.last_slice_agents = applied;
            self.progress.pending_agents = self.pending.len();
        }
        self.progress.last_slice_agents = applied;
        self.progress.pending_agents = self.pending.len();
        applied
    }

    fn drain_slice(&mut self) -> usize {
        let started = Instant::now();
        self.drain_with(
            |row| {
                let project = row.project_key.trim();
                engine::atc_sync_agent_snapshot(
                    &row.name,
                    &row.program,
                    (!project.is_empty()).then_some(project),
                    row.last_active_ts,
                );
            },
            || started.elapsed() < HYDRATION_SLICE,
        )
    }

    fn annotate(&self, summary: &mut AtcSummarySnapshot, now_micros: i64) {
        if self.resume_pending {
            annotate_pending_summary(summary, now_micros);
        }
    }
}

fn annotate_pending_summary(summary: &mut AtcSummarySnapshot, now_micros: i64) {
    summary.completeness = engine::SnapshotCompleteness::Partial;
    summary.kernel.next_due_micros = Some(now_micros);
    summary.kernel.due_agents = 0;
    summary.kernel.pending_effects = 0;
    summary.policy.fallback_active = true;
    summary.policy.fallback_reason = Some("population_hydration_incomplete".to_string());
}

static HYDRATION: OnceLock<Mutex<HydrationState>> = OnceLock::new();

fn hydration() -> &'static Mutex<HydrationState> {
    HYDRATION.get_or_init(|| Mutex::new(HydrationState::default()))
}

/// Serialize reset with refresh/tick so an old snapshot cannot seed a new engine.
pub(super) fn reset_with(reset_engine: impl FnOnce()) {
    let mut state = hydration().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    reset_engine();
    *state = HydrationState::default();
}

#[must_use]
pub fn atc_population_hydration_stats() -> AtcPopulationHydrationStats {
    hydration().lock().unwrap_or_else(std::sync::PoisonError::into_inner).progress
}

/// Fetch the bounded recent population and apply at most one hydration slice.
///
/// Returned counts describe the database snapshot, not completion of all engine
/// updates. Small snapshots finish here; larger ones continue at the public tick
/// boundary. An unfinished refresh is retained rather than restarted. Unchanged
/// rows in subsequent complete snapshots do not touch the engine at all.
pub fn atc_sync_population_from_db(
    pool: &mcp_agent_mail_db::DbPool,
) -> Result<AtcPopulationSyncStats, String> {
    let mut state = hydration().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !engine::atc_enabled() {
        return Ok(AtcPopulationSyncStats::default());
    }
    let now = mcp_agent_mail_core::timestamps::now_micros();
    let stats = state.refresh_with(now, || {
        let recency_micros = i64::try_from(mcp_agent_mail_core::config::atc_population_recency_secs())
            .unwrap_or(i64::MAX)
            .saturating_mul(1_000_000);
        let population_limit = mcp_agent_mail_core::config::atc_population_limit();
        let cx = asupersync::Cx::for_request_with_budget(asupersync::Budget::INFINITE);
        match fastmcp_core::block_on(mcp_agent_mail_db::queries::list_atc_population_snapshot(
            &cx,
            pool,
            now.saturating_sub(recency_micros),
            population_limit,
        )) {
            asupersync::Outcome::Ok(rows) => Ok(rows),
            asupersync::Outcome::Err(error) => Err(error.to_string()),
            asupersync::Outcome::Cancelled(reason) => Err(format!("cancelled: {reason:?}")),
            asupersync::Outcome::Panicked(payload) => Err(format!("panicked: {}", payload.message())),
        }
    })?;
    state.drain_slice();
    Ok(stats)
}

pub(super) fn tick_report(now_micros: i64) -> Option<AtcTickReport> {
    // The lock serializes only operator-side work. Request-side activity hooks
    // still go directly to the engine and never acquire this hydration lock.
    let mut state = hydration().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !engine::atc_enabled() {
        return None;
    }
    if state.pending.is_empty() {
        state.resume_pending = false;
        state.progress.last_slice_agents = 0;
        return engine::atc_tick_report(now_micros);
    }
    let started = Instant::now();
    state.drain_slice();
    let mut summary = engine::atc_summary()?;
    // Even the final slice yields before inference. Otherwise its work would
    // stack with the kernel budget, and a slow last batch would never yield.
    state.annotate(&mut summary, now_micros);
    let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    summary.stage_timings = engine::AtcStageTimings {
        total_micros: elapsed,
        ..engine::AtcStageTimings::default()
    };
    summary.budget.kernel_total_micros = elapsed;
    // The engine may not have produced its first budget snapshot yet.
    if summary.budget.tick_budget_micros == 0 {
        summary.budget.tick_budget_micros = engine::AtcConfig::default().tick_budget_micros;
    }
    #[allow(clippy::cast_precision_loss)]
    {
        summary.budget.utilization_ratio = elapsed as f64 / summary.budget.tick_budget_micros.max(1) as f64;
    }
    Some(AtcTickReport { actions: Vec::new(), effects: Vec::new(), summary })
}

pub(super) fn annotate_summary(summary: &mut AtcSummarySnapshot) {
    let now = mcp_agent_mail_core::timestamps::now_micros();
    // Snapshot readers must not join an operator-side database wait.
    match hydration().try_lock() {
        Ok(state) => state.annotate(summary, now),
        Err(TryLockError::WouldBlock) => annotate_pending_summary(summary, now),
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().annotate(summary, now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(count: usize) -> Vec<AtcPopulationAgentRow> {
        (0..count).map(|index| AtcPopulationAgentRow {
            project_id: 1,
            project_key: "/hydration".into(),
            name: format!("HydratedAgent{index:04}"),
            program: "codex-cli".into(),
            last_active_ts: 1_000_000,
        }).collect()
    }

    #[test]
    fn population_940_drains_in_bounded_fifo_slices_without_loss() {
        let input = rows(940);
        let mut state = HydrationState::default();
        let stats = state.refresh_with(10, || Ok(input.clone())).unwrap();
        assert_eq!(stats.agents, 940);
        let mut applied = Vec::new();
        let mut slices = 0;
        while !state.pending.is_empty() {
            let count = state.drain_with(|row| applied.push(row.clone()), || true);
            assert!((1..=MAX_AGENTS_PER_SLICE).contains(&count));
            assert_eq!(state.progress.applied_agents + state.progress.pending_agents, 940);
            slices += 1;
        }
        assert_eq!(applied, input);
        assert_eq!(slices, 940_usize.div_ceil(MAX_AGENTS_PER_SLICE));
        assert_eq!(state.previous_snapshot.len(), 940);
    }

    #[test]
    fn exhausted_time_budget_still_makes_one_row_of_progress() {
        let mut state = HydrationState::default();
        state.refresh_with(10, || Ok(rows(940))).unwrap();
        assert_eq!(state.drain_with(|_| {}, || false), 1);
        assert_eq!(state.progress.pending_agents, 939);
    }

    #[test]
    fn repeated_refresh_does_not_reload_or_restart_an_unfinished_snapshot() {
        let mut state = HydrationState::default();
        state.refresh_with(10, || Ok(rows(940))).unwrap();
        state.drain_with(|_| {}, || true);
        for _ in 0..10 {
            let stats = state.refresh_with(20, || panic!("must not re-query pending snapshot")).unwrap();
            assert_eq!(stats.agents, 940);
        }
        assert_eq!(state.pending.front().unwrap().name, "HydratedAgent0032");
        assert_eq!(state.progress.deferred_refreshes, 10);
    }

    #[test]
    fn unchanged_periodic_snapshot_performs_no_engine_updates() {
        let mut state = HydrationState::default();
        state.refresh_with(10, || Ok(rows(940))).unwrap();
        while !state.pending.is_empty() {
            state.drain_with(|_| {}, || true);
        }
        state.refresh_with(20, || Ok(rows(940))).unwrap();
        assert_eq!(state.progress.unchanged_agents, 940);
        assert_eq!(state.progress.pending_agents, 0);
        assert_eq!(state.drain_with(|_| panic!("unchanged row reached engine"), || true), 0);
        assert!(!state.resume_pending);
    }

    #[test]
    fn changed_activity_program_and_project_are_not_hidden_by_cache() {
        let mut state = HydrationState::default();
        state.refresh_with(10, || Ok(rows(4))).unwrap();
        state.drain_with(|_| {}, || true);
        let mut changed = rows(4);
        changed[0].last_active_ts += 1;
        changed[1].program = "claude-code".into();
        changed[2].project_key = "/renamed-project".into();
        state.refresh_with(20, || Ok(changed)).unwrap();
        assert_eq!(state.progress.unchanged_agents, 1);
        assert_eq!(state.progress.pending_agents, 3);
    }

    #[test]
    fn snapshot_cache_does_not_accumulate_departed_names() {
        let mut state = HydrationState::default();
        state.refresh_with(10, || Ok(rows(940))).unwrap();
        while !state.pending.is_empty() {
            state.drain_with(|_| {}, || true);
        }
        state.refresh_with(20, || Ok(rows(1))).unwrap();
        assert_eq!(state.previous_snapshot.len(), 1);
        state.refresh_with(30, || Ok(Vec::new())).unwrap();
        assert!(state.previous_snapshot.is_empty());
        assert_eq!(state.snapshot_stats, AtcPopulationSyncStats::default());
    }

    #[test]
    fn failed_query_preserves_previous_snapshot_and_progress() {
        let mut state = HydrationState::default();
        state.refresh_with(10, || Ok(rows(1))).unwrap();
        state.drain_with(|_| {}, || true);
        let before = state.progress;
        assert!(state.refresh_with(20, || Err("injected database error".into())).is_err());
        assert_eq!(state.progress, before);
        assert_eq!(state.previous_snapshot.len(), 1);
    }

    #[test]
    fn interrupted_apply_keeps_the_head_for_monotonic_retry() {
        let mut state = HydrationState::default();
        state.refresh_with(10, || Ok(rows(2))).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.drain_with(|_| panic!("injected engine update failure"), || true);
        }));
        assert!(result.is_err());
        assert_eq!(state.pending.front().unwrap().name, "HydratedAgent0000");
        assert_eq!(state.progress.applied_agents, 0);
        assert_eq!(state.drain_with(|_| {}, || true), 2);
    }

    #[test]
    fn hydration_defers_inference_and_keeps_a_resume_deadline_through_the_final_slice() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config { atc_enabled: true, ..Default::default() };
        super::super::reset_global_atc_state_for_test(&config);
        hydration().lock().unwrap().refresh_with(10, || Ok(rows(65))).unwrap();
        let mut steps = 0;
        loop {
            let report = super::super::atc_tick_report(2_000_000).unwrap();
            steps += 1;
            if report.summary.completeness != engine::SnapshotCompleteness::Partial {
                break;
            }
            assert!(report.effects.is_empty());
            assert!(report.actions.is_empty());
            assert_eq!(report.summary.kernel.next_due_micros, Some(2_000_000));
            assert_eq!(report.summary.tick_count, 0, "inference ran on a partial population");
            assert!(atc_population_hydration_stats().last_slice_agents <= MAX_AGENTS_PER_SLICE);
            let summary = super::super::atc_summary().unwrap();
            assert_eq!(summary.completeness, engine::SnapshotCompleteness::Partial);
            assert!(steps <= 66, "hydration stopped making progress");
        }
        assert_eq!(engine::atc_summary().unwrap().tracked_agents.len(), 65);
        assert_eq!(atc_population_hydration_stats().pending_agents, 0);
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn new_tool_activity_is_not_regressed_by_a_queued_old_snapshot() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config { atc_enabled: true, ..Default::default() };
        super::super::reset_global_atc_state_for_test(&config);
        hydration().lock().unwrap().refresh_with(10, || Ok(rows(65))).unwrap();
        engine::atc_observe_activity_with_project("HydratedAgent0064", Some("/hydration"), 9_000_000);
        while atc_population_hydration_stats().pending_agents > 0 {
            let _ = super::super::atc_tick_report(10_000_000).unwrap();
        }
        assert_eq!(engine::atc_agent_last_activity("HydratedAgent0064"), Some(9_000_000));
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn public_reset_discards_old_hydration_and_unchanged_cache() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config { atc_enabled: true, ..Default::default() };
        super::super::reset_global_atc_state_for_test(&config);
        hydration().lock().unwrap().refresh_with(10, || Ok(rows(940))).unwrap();
        super::super::reset_global_atc_state_for_test(&config);
        assert_eq!(atc_population_hydration_stats(), AtcPopulationHydrationStats::default());
        assert!(hydration().lock().unwrap().previous_snapshot.is_empty());
        assert!(super::super::atc_tick_report(2_000_000).unwrap().summary.tracked_agents.is_empty());
    }

    #[test]
    fn summary_does_not_wait_for_an_operator_population_query_lock() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config { atc_enabled: true, ..Default::default() };
        super::super::reset_global_atc_state_for_test(&config);
        let _query_guard = hydration().lock().unwrap();
        let summary = super::super::atc_summary().unwrap();
        assert_eq!(summary.completeness, engine::SnapshotCompleteness::Partial);
        assert_eq!(summary.policy.fallback_reason.as_deref(), Some("population_hydration_incomplete"));
    }
}
