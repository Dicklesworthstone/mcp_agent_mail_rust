//! Incremental population hydration for the public ATC operator boundary.
//!
//! The database query remains a single bounded projection. Applying that
//! projection to the inference engine is separate work: each registration can
//! update schedules and cohort state. Never apply thousands of rows in one tick.
//! While a refresh is incomplete, publish a partial snapshot and defer inference
//! instead of releasing reservations against an only-partially-refreshed roster.
//! A failed refresh also blocks inference until a new database read succeeds:
//! inability to observe activity is not evidence that an agent has died.
//! Once a database population has been installed, remembered engine agents
//! outside that population cannot authorize automatic effects or legacy actions.

use super::engine::{self, AtcPopulationSyncStats, AtcSummarySnapshot, AtcTickReport};
use mcp_agent_mail_db::models::AtcPopulationAgentRow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};

#[path = "atc_population_scope.rs"]
mod scope;
use scope::PopulationScope;

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
    /// Failed or interrupted population refreshes since the last engine reset.
    pub refresh_failures: u64,
    /// Inference is suspended until a subsequent refresh succeeds.
    pub refresh_failed: bool,
    /// A population read is running without holding the hydration mutex.
    pub refresh_in_flight: bool,
    /// Invalid or multi-project names in the current bounded population.
    pub scope_unresolved_names: usize,
    /// Lifetime proposal/action counts withheld by population scope, not delivery failures.
    pub scope_effects_withheld: u64,
    pub scope_actions_withheld: u64,
    pub last_scope_effects_withheld: usize,
    pub last_scope_actions_withheld: usize,
}

#[derive(Default)]
struct HydrationState {
    pending: VecDeque<AtcPopulationAgentRow>,
    // Bounded by the most recent query's population limit, not lifetime names.
    // A new refresh cannot replace an unfinished one, so this represents fully
    // applied rows whenever it is consulted for unchanged-row suppression.
    previous_snapshot: HashMap<AgentKey, AtcPopulationAgentRow>,
    // None preserves standalone engine use before the first DB population.
    // Some(empty) is different: a successful empty read withdraws all automatic
    // target authority. Failed or unfinished refreshes already suspend inference.
    scope: Option<PopulationScope>,
    snapshot_stats: AtcPopulationSyncStats,
    progress: AtcPopulationHydrationStats,
    resume_pending: bool,
    // Identity, not a wrapping generation counter: a reset may start another
    // read while an old one is still returning. The old lease keeps its Arc
    // alive, so an allocator cannot reuse that identity for the new read.
    active_refresh: Option<Arc<()>>,
}

impl HydrationState {
    // Not `const fn`: `VecDeque::is_empty` is not callable in a const context
    // (E0015), and marking this const breaks the build outright.
    fn should_defer_refresh(&self) -> bool {
        self.active_refresh.is_some()
            || !self.pending.is_empty()
            // The final hydration slice deliberately yields before inference.
            // Do not let another changed snapshot steal that inference turn
            // and keep the operator hydrating forever. A failed refresh is
            // different: it must retry before inference is safe again.
            || (self.resume_pending && !self.progress.refresh_failed)
    }

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
        // Arm the safety gate before calling the loader. If it unwinds rather
        // than returning an error, poison recovery must not resume inference
        // against the old roster. Keep the last good snapshot for retries.
        self.progress.refresh_failed = true;
        let rows = match load() {
            Ok(rows) => rows,
            Err(error) => {
                self.progress.refresh_failures = self.progress.refresh_failures.saturating_add(1);
                return Err(error);
            }
        };
        // Build scope from the actual projection before deduplicating cache
        // keys. Conflicting duplicate identities must not become last-row-wins.
        let next_scope = PopulationScope::from_rows(&rows);
        let unresolved_names = next_scope.unresolved_names();
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
        self.scope = Some(next_scope);
        self.pending = pending;
        self.snapshot_stats = stats;
        self.progress = AtcPopulationHydrationStats {
            snapshot_agents: stats.agents,
            pending_agents: self.pending.len(),
            unchanged_agents: unchanged,
            deferred_refreshes: self.progress.deferred_refreshes,
            snapshot_started_at_micros: now_micros,
            refresh_failures: self.progress.refresh_failures,
            scope_unresolved_names: unresolved_names,
            scope_effects_withheld: self.progress.scope_effects_withheld,
            scope_actions_withheld: self.progress.scope_actions_withheld,
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

    fn filter_report_scope(&mut self, report: &mut AtcTickReport) {
        let Some(scope) = self.scope.as_ref() else {
            return;
        };
        let disposition = scope.retain_authorized(&mut report.effects, &mut report.actions);
        self.progress.last_scope_effects_withheld = disposition.effects_withheld;
        self.progress.last_scope_actions_withheld = disposition.actions_withheld;
        self.progress.scope_effects_withheld = self.progress.scope_effects_withheld.saturating_add(
            u64::try_from(disposition.effects_withheld).unwrap_or(u64::MAX),
        );
        self.progress.scope_actions_withheld = self.progress.scope_actions_withheld.saturating_add(
            u64::try_from(disposition.actions_withheld).unwrap_or(u64::MAX),
        );
        report.summary.kernel.pending_effects = report.effects.len();
    }

    fn annotate(&self, summary: &mut AtcSummarySnapshot, now_micros: i64) {
        if self.active_refresh.is_some() {
            annotate_incomplete_summary(summary, None, "population_refresh_in_progress");
        } else if self.progress.refresh_failed {
            // No immediate inference deadline: only a successful population
            // refresh can clear this gate. Repeated ticks cannot repair a DB
            // failure and must not turn it into a busy retry loop.
            annotate_incomplete_summary(summary, None, "population_refresh_failed");
        } else if self.resume_pending {
            annotate_incomplete_summary(
                summary,
                Some(now_micros),
                "population_hydration_incomplete",
            );
        } else if self.progress.scope_unresolved_names > 0
            || self.progress.last_scope_effects_withheld > 0
            || self.progress.last_scope_actions_withheld > 0
        {
            // Unambiguous in-scope proposals may continue. Do not report full
            // coverage, invent delivery failures, or overwrite a stronger guard
            // reason while stale/ambiguous engine proposals are being withheld.
            summary.completeness = engine::SnapshotCompleteness::Partial;
            summary.policy.fallback_active = true;
            if summary.policy.fallback_reason.is_none() {
                summary.policy.fallback_reason = Some("population_scope_restricted".to_string());
            }
        }
    }
}

fn annotate_incomplete_summary(
    summary: &mut AtcSummarySnapshot,
    next_due_micros: Option<i64>,
    reason: &str,
) {
    summary.completeness = engine::SnapshotCompleteness::Partial;
    summary.kernel.next_due_micros = next_due_micros;
    summary.kernel.due_agents = 0;
    summary.kernel.pending_effects = 0;
    summary.policy.fallback_active = true;
    summary.policy.fallback_reason = Some(reason.to_string());
}

static HYDRATION: OnceLock<Mutex<HydrationState>> = OnceLock::new();

fn hydration() -> &'static Mutex<HydrationState> {
    HYDRATION.get_or_init(|| Mutex::new(HydrationState::default()))
}

/// A query owns no engine/hydration lock. This lease only permits its result to
/// enter the epoch that admitted it; dropping an interrupted query fails closed.
struct PopulationRefresh {
    token: Arc<()>,
}

impl PopulationRefresh {
    fn start(state: &mut HydrationState) -> Self {
        let token = Arc::new(());
        state.active_refresh = Some(Arc::clone(&token));
        state.progress.refresh_in_flight = true;
        Self { token }
    }

    fn is_current(&self, state: &HydrationState) -> bool {
        state
            .active_refresh
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &self.token))
    }
}

impl Drop for PopulationRefresh {
    fn drop(&mut self) {
        let mut state = hydration()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_current(&state) {
            state.active_refresh = None;
            state.progress.refresh_in_flight = false;
            state.progress.refresh_failed = true;
            state.progress.refresh_failures = state.progress.refresh_failures.saturating_add(1);
        }
    }
}

/// Serialize reset with refresh/tick so an old snapshot cannot seed a new engine.
/// In-flight database reads do not delay reset; their leases are invalidated.
pub(super) fn reset_with(reset_engine: impl FnOnce()) {
    let mut state = hydration()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reset_engine();
    *state = HydrationState::default();
}

#[must_use]
pub fn atc_population_hydration_stats() -> AtcPopulationHydrationStats {
    hydration()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .progress
}

/// Fetch the bounded recent population and apply at most one hydration slice.
///
/// Returned counts describe the database snapshot, not completion of all engine
/// updates. Small snapshots finish here; larger ones continue at the public tick
/// boundary. An unfinished refresh is retained rather than restarted. Unchanged
/// rows in subsequent complete snapshots do not touch the engine at all.
/// After hydration finishes, the public tick gets one inference turn before a
/// new refresh is admitted. Frequent changing snapshots cannot starve it.
/// A failed read suspends inference, without clearing the last good snapshot,
/// until a later read succeeds. Errors are still returned to the operator.
/// Every installed DB population also replaces the scope for automatic effects;
/// missing agents are withheld, never classified as dead or deleted by omission.
pub fn atc_sync_population_from_db(
    pool: &mcp_agent_mail_db::DbPool,
) -> Result<AtcPopulationSyncStats, String> {
    let now = mcp_agent_mail_core::timestamps::now_micros();
    sync_population_with(now, || {
        let recency_micros =
            i64::try_from(mcp_agent_mail_core::config::atc_population_recency_secs())
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
            asupersync::Outcome::Panicked(payload) => {
                Err(format!("panicked: {}", payload.message()))
            }
        }
    })
}

fn sync_population_with(
    now_micros: i64,
    load: impl FnOnce() -> Result<Vec<AtcPopulationAgentRow>, String>,
) -> Result<AtcPopulationSyncStats, String> {
    let refresh = {
        let mut state = hydration()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !engine::atc_enabled() {
            return Ok(AtcPopulationSyncStats::default());
        }
        if state.should_defer_refresh() {
            state.progress.deferred_refreshes = state.progress.deferred_refreshes.saturating_add(1);
            if state.active_refresh.is_none() {
                state.drain_slice();
            }
            return Ok(state.snapshot_stats);
        }
        PopulationRefresh::start(&mut state)
    };

    // The DB may wait on a connection or an I/O operation. Never make summary,
    // progress, tick or reset callers join that wait through the hydration lock.
    // Inference remains passive while the query owns the refresh lease.
    let loaded = load();

    let mut state = hydration()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !refresh.is_current(&state) {
        return Err("population refresh discarded after ATC reset".to_string());
    }
    // Only the already-loaded result crosses this short critical section.
    let result = state.refresh_with(now_micros, || loaded);
    state.active_refresh = None;
    state.progress.refresh_in_flight = false;
    if result.is_ok() {
        state.drain_slice();
    }
    // `state` was declared after `refresh`, so it is dropped before the lease.
    // On an unwind during installation, the lease recovers the poisoned lock
    // and leaves the epoch failed rather than stranding a single-flight token.
    result
}

pub(super) fn tick_report(now_micros: i64) -> Option<AtcTickReport> {
    // The lock serializes only operator-side work. Request-side activity hooks
    // still go directly to the engine and never acquire this hydration lock.
    let mut state = hydration()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !engine::atc_enabled() {
        return None;
    }
    let started = Instant::now();
    if state.active_refresh.is_some() || state.progress.refresh_failed {
        state.progress.last_slice_agents = 0;
        return partial_report(&state, now_micros, started);
    }
    if state.pending.is_empty() {
        state.resume_pending = false;
        state.progress.last_slice_agents = 0;
        let mut report = engine::atc_tick_report(now_micros)?;
        // Validate while holding the same lease that installs population scope.
        // The delivery gate later budgets optional mail; it cannot grant scope.
        state.filter_report_scope(&mut report);
        state.annotate(&mut report.summary, now_micros);
        return Some(report);
    }
    state.drain_slice();
    partial_report(&state, now_micros, started)
}

fn partial_report(
    state: &HydrationState,
    now_micros: i64,
    started: Instant,
) -> Option<AtcTickReport> {
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
        summary.budget.utilization_ratio =
            elapsed as f64 / summary.budget.tick_budget_micros.max(1) as f64;
    }
    Some(AtcTickReport {
        actions: Vec::new(),
        effects: Vec::new(),
        summary,
    })
}

pub(super) fn annotate_summary(summary: &mut AtcSummarySnapshot) {
    let now = mcp_agent_mail_core::timestamps::now_micros();
    // Snapshot readers must not join an operator-side database wait.
    match hydration().try_lock() {
        Ok(state) => state.annotate(summary, now),
        Err(TryLockError::WouldBlock) => {
            annotate_incomplete_summary(summary, Some(now), "population_hydration_incomplete");
        }
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().annotate(summary, now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(count: usize) -> Vec<AtcPopulationAgentRow> {
        (0..count)
            .map(|index| AtcPopulationAgentRow {
                project_id: 1,
                project_key: "/hydration".into(),
                name: format!("HydratedAgent{index:04}"),
                program: "codex-cli".into(),
                last_active_ts: 1_000_000,
            })
            .collect()
    }

    #[test]
    fn population_940_drains_in_bounded_fifo_slices_without_loss() {
        let input = rows(940);
        let mut state = HydrationState::default();
        let sync_stats = state.refresh_with(10, || Ok(input.clone())).unwrap();
        assert_eq!(sync_stats.agents, 940);
        let mut applied = Vec::new();
        let mut slices = 0;
        while !state.pending.is_empty() {
            let count = state.drain_with(|row| applied.push(row.clone()), || true);
            assert!((1..=MAX_AGENTS_PER_SLICE).contains(&count));
            assert_eq!(
                state.progress.applied_agents + state.progress.pending_agents,
                940
            );
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
            let sync_stats = state
                .refresh_with(20, || panic!("must not re-query pending snapshot"))
                .unwrap();
            assert_eq!(sync_stats.agents, 940);
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
        assert_eq!(
            state.drain_with(|_| panic!("unchanged row reached engine"), || true),
            0
        );
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
        assert!(
            state
                .refresh_with(20, || Err("injected database error".into()))
                .is_err()
        );
        assert_eq!(
            state.progress,
            AtcPopulationHydrationStats {
                refresh_failures: 1,
                refresh_failed: true,
                ..before
            },
        );
        assert_eq!(state.previous_snapshot.len(), 1);
    }

    #[test]
    fn refresh_failure_count_saturates_and_success_preserves_its_history() {
        let mut state = HydrationState::default();
        state.progress.refresh_failures = u64::MAX - 1;
        for _ in 0..3 {
            assert!(
                state
                    .refresh_with(10, || Err("database unavailable".into()))
                    .is_err()
            );
            assert!(state.progress.refresh_failed);
            assert_eq!(state.progress.refresh_failures, u64::MAX);
        }
        state.refresh_with(20, || Ok(Vec::new())).unwrap();
        assert!(!state.progress.refresh_failed);
        assert_eq!(state.progress.refresh_failures, u64::MAX);
    }

    #[test]
    fn loader_unwind_leaves_inference_blocked_and_the_last_good_snapshot_intact() {
        let mut state = HydrationState::default();
        state.refresh_with(10, || Ok(rows(1))).unwrap();
        state.drain_with(|_| {}, || true);
        let before = state.previous_snapshot.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = state.refresh_with(20, || panic!("population loader panicked"));
        }));
        assert!(result.is_err());
        assert!(state.progress.refresh_failed);
        assert_eq!(state.previous_snapshot, before);
        state.refresh_with(30, || Ok(rows(1))).unwrap();
        assert!(!state.progress.refresh_failed);
        assert_eq!(state.progress.unchanged_agents, 1);
    }

    #[test]
    fn population_read_releases_hydration_lock_but_keeps_public_ticks_passive() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        let sync_stats = sync_population_with(10, || {
            {
                let state = hydration().try_lock().expect("DB read held hydration lock");
                assert!(state.progress.refresh_in_flight);
                assert!(state.active_refresh.is_some());
            }
            let report = super::super::atc_tick_report(2_000_000).unwrap();
            assert_eq!(report.summary.tick_count, 0);
            assert_eq!(report.actions.len(), 0);
            assert_eq!(report.effects.len(), 0);
            assert_eq!(report.summary.kernel.next_due_micros, None);
            assert_eq!(
                report.summary.policy.fallback_reason.as_deref(),
                Some("population_refresh_in_progress"),
            );
            assert!(atc_population_hydration_stats().refresh_in_flight);
            assert_eq!(
                super::super::atc_summary()
                    .unwrap()
                    .policy
                    .fallback_reason
                    .as_deref(),
                Some("population_refresh_in_progress"),
            );
            Ok(rows(1))
        })
        .unwrap();
        assert_eq!(sync_stats.agents, 1);
        let progress = atc_population_hydration_stats();
        assert!(!progress.refresh_in_flight);
        assert!(!progress.refresh_failed);
        assert_eq!(progress.applied_agents, 1);
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn concurrent_refresh_defers_without_waiting_for_or_repeating_the_query() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        let (send, receive) = std::sync::mpsc::channel();
        let mut worker = None;
        let mut completed_during_load = None;
        let result = sync_population_with(10, || {
            worker = Some(std::thread::spawn(move || {
                let deferred = sync_population_with(20, || panic!("duplicate population query"));
                let _ = send.send(deferred);
            }));
            completed_during_load = receive.recv_timeout(Duration::from_secs(5)).ok();
            // Finish the first read before asserting or joining. Reintroducing
            // the old long-held mutex fails this test without stranding a thread.
            Ok(rows(1))
        });
        worker.unwrap().join().unwrap();
        assert!(result.is_ok());
        assert_eq!(
            completed_during_load,
            Some(Ok(AtcPopulationSyncStats::default()))
        );
        assert_eq!(atc_population_hydration_stats().deferred_refreshes, 1);
        assert_eq!(atc_population_hydration_stats().applied_agents, 1);
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn completed_hydration_defers_a_new_read_until_the_public_tick_runs() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        sync_population_with(10, || Ok(rows(1))).unwrap();
        assert_eq!(atc_population_hydration_stats().pending_agents, 0);
        assert!(hydration().lock().unwrap().resume_pending);
        let cached = sync_population_with(20, || panic!("refresh stole the inference turn"));
        assert_eq!(cached.unwrap().agents, 1);
        assert_eq!(atc_population_hydration_stats().deferred_refreshes, 1);
        assert_eq!(engine::atc_summary().unwrap().tick_count, 0);
        assert_eq!(
            super::super::atc_tick_report(2_000_000)
                .unwrap()
                .summary
                .tick_count,
            1
        );
        assert!(!hydration().lock().unwrap().resume_pending);

        let mut loaded = false;
        sync_population_with(30, || {
            loaded = true;
            let mut changed = rows(1);
            changed[0].last_active_ts = 3_000_000;
            Ok(changed)
        })
        .unwrap();
        assert!(loaded, "completed inference did not allow the next refresh");
        assert_eq!(
            engine::atc_agent_last_activity("HydratedAgent0000"),
            Some(3_000_000)
        );
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn failed_refresh_can_recover_even_with_an_outstanding_inference_turn() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        sync_population_with(10, || Ok(rows(1))).unwrap();
        {
            let mut state = hydration().lock().unwrap();
            assert!(state.resume_pending);
            assert!(
                state
                    .refresh_with(20, || Err("lost activity evidence".into()))
                    .is_err()
            );
            assert!(!state.should_defer_refresh());
        }
        assert_eq!(
            super::super::atc_tick_report(2_000_000)
                .unwrap()
                .summary
                .tick_count,
            0
        );
        let mut loaded = false;
        sync_population_with(30, || {
            loaded = true;
            Ok(rows(1))
        })
        .unwrap();
        assert!(loaded);
        assert!(!atc_population_hydration_stats().refresh_failed);
        assert_eq!(
            super::super::atc_tick_report(3_000_000)
                .unwrap()
                .summary
                .tick_count,
            1
        );
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn refresh_gate_preserves_inference_turns_in_a_940_agent_cadence() {
        // Exercise the actual admission gate and bounded queue transitions
        // deterministically; the public-tick test above covers its engine edge.
        let mut state = HydrationState::default();
        let mut inference_turns = 0;
        let mut reads = 0;
        for step in 1..=300 {
            if !state.should_defer_refresh() {
                let mut changed = rows(940);
                for row in &mut changed {
                    row.last_active_ts += step;
                }
                state.refresh_with(step, || Ok(changed)).unwrap();
                reads += 1;
            }
            // A refresh call may drain one slice; the subsequent public tick
            // either infers on an empty queue or drains and yields, even when
            // that tick consumes the final slice.
            state.drain_with(|_| {}, || true);
            if state.pending.is_empty() {
                state.resume_pending = false;
                inference_turns += 1;
            } else {
                state.drain_with(|_| {}, || true);
            }
        }
        assert_eq!(inference_turns, 18);
        assert_eq!(reads, 19);
    }

    #[test]
    fn reset_during_read_rejects_late_rows_without_touching_the_new_epoch() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        let old = sync_population_with(10, || {
            super::super::reset_global_atc_state_for_test(&config);
            sync_population_with(20, || {
                let mut fresh = rows(1);
                fresh[0].name = "FreshEpochAgent".into();
                Ok(fresh)
            })
            .unwrap();
            Ok(rows(65))
        });
        assert_eq!(
            old.unwrap_err(),
            "population refresh discarded after ATC reset"
        );
        let progress = atc_population_hydration_stats();
        assert_eq!(progress.snapshot_started_at_micros, 20);
        assert_eq!(progress.snapshot_agents, 1);
        assert_eq!(progress.applied_agents, 1);
        assert_eq!(progress.refresh_failures, 0);
        assert!(!progress.refresh_failed);
        assert!(!progress.refresh_in_flight);
        assert_eq!(
            engine::atc_agent_last_activity("FreshEpochAgent"),
            Some(1_000_000)
        );
        assert_eq!(engine::atc_agent_last_activity("HydratedAgent0000"), None);
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn late_query_error_cannot_poison_a_successful_post_reset_refresh() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        let old = sync_population_with(10, || {
            super::super::reset_global_atc_state_for_test(&config);
            sync_population_with(20, || Ok(rows(1))).unwrap();
            Err("failure belongs to the previous epoch".to_string())
        });
        assert!(old.is_err());
        let progress = atc_population_hydration_stats();
        assert_eq!(progress.snapshot_started_at_micros, 20);
        assert_eq!(progress.refresh_failures, 0);
        assert!(!progress.refresh_failed);
        assert!(!progress.refresh_in_flight);
        assert_eq!(
            super::super::atc_tick_report(2_000_000)
                .unwrap()
                .summary
                .tick_count,
            1
        );
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn panicking_query_releases_single_flight_lease_and_allows_a_fresh_read() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        let interrupted = std::panic::catch_unwind(|| {
            let _ = sync_population_with(10, || panic!("interrupted population query"));
        });
        assert!(interrupted.is_err());
        let progress = atc_population_hydration_stats();
        assert!(!progress.refresh_in_flight);
        assert!(progress.refresh_failed);
        assert_eq!(progress.refresh_failures, 1);
        assert_eq!(
            super::super::atc_tick_report(2_000_000)
                .unwrap()
                .summary
                .tick_count,
            0
        );
        sync_population_with(20, || Ok(rows(1))).unwrap();
        assert!(!atc_population_hydration_stats().refresh_failed);
        assert_eq!(atc_population_hydration_stats().applied_agents, 1);
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn dropping_an_old_lease_does_not_cancel_a_newer_query() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        let old = PopulationRefresh::start(&mut hydration().lock().unwrap());
        super::super::reset_global_atc_state_for_test(&config);
        let fresh = PopulationRefresh::start(&mut hydration().lock().unwrap());
        drop(old);
        {
            let state = hydration().lock().unwrap();
            assert!(fresh.is_current(&state));
            assert!(state.progress.refresh_in_flight);
            assert_eq!(state.progress.refresh_failures, 0);
        }
        drop(fresh);
        assert!(!atc_population_hydration_stats().refresh_in_flight);
        assert_eq!(atc_population_hydration_stats().refresh_failures, 1);
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn failed_refresh_blocks_public_inference_until_an_unchanged_read_succeeds() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        {
            let mut state = hydration().lock().unwrap();
            state.refresh_with(10, || Ok(rows(1))).unwrap();
            state.drain_slice();
        }
        let before = super::super::atc_tick_report(2_000_000)
            .unwrap()
            .summary
            .tick_count;
        assert!(
            hydration()
                .lock()
                .unwrap()
                .refresh_with(20, || Err("database unavailable".into()))
                .is_err()
        );

        for now in [3_000_000, 60_000_000, 86_400_000_000] {
            let report = super::super::atc_tick_report(now).unwrap();
            assert_eq!(report.summary.tick_count, before);
            assert_eq!(report.actions.len(), 0);
            assert_eq!(report.effects.len(), 0);
            assert_eq!(
                report.summary.completeness,
                engine::SnapshotCompleteness::Partial
            );
            assert_eq!(report.summary.kernel.next_due_micros, None);
            assert_eq!(
                report.summary.policy.fallback_reason.as_deref(),
                Some("population_refresh_failed"),
            );
            let summary = super::super::atc_summary().unwrap();
            assert_eq!(
                summary.policy.fallback_reason.as_deref(),
                Some("population_refresh_failed")
            );
        }

        hydration()
            .lock()
            .unwrap()
            .refresh_with(30, || Ok(rows(1)))
            .unwrap();
        let progress = atc_population_hydration_stats();
        assert_eq!(progress.unchanged_agents, 1);
        assert_eq!(progress.pending_agents, 0);
        assert_eq!(progress.refresh_failures, 1);
        assert!(!progress.refresh_failed);
        assert!(
            super::super::atc_tick_report(86_401_000_000)
                .unwrap()
                .summary
                .tick_count
                > before
        );
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn recovery_with_changed_rows_still_yields_until_hydration_finishes() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        {
            let mut state = hydration().lock().unwrap();
            assert!(
                state
                    .refresh_with(10, || Err("cold-start failure".into()))
                    .is_err()
            );
        }
        assert_eq!(
            super::super::atc_tick_report(2_000_000)
                .unwrap()
                .summary
                .tick_count,
            0
        );
        hydration()
            .lock()
            .unwrap()
            .refresh_with(20, || Ok(rows(65)))
            .unwrap();
        assert!(!atc_population_hydration_stats().refresh_failed);
        let mut slices = 0;
        while atc_population_hydration_stats().pending_agents > 0 {
            let report = super::super::atc_tick_report(3_000_000).unwrap();
            assert_eq!(report.summary.tick_count, 0);
            assert_eq!(report.effects.len(), 0);
            assert_eq!(
                report.summary.completeness,
                engine::SnapshotCompleteness::Partial
            );
            slices += 1;
            assert!(slices <= 65, "recovery hydration stopped making progress");
        }
        assert_eq!(
            super::super::atc_tick_report(4_000_000)
                .unwrap()
                .summary
                .tick_count,
            1
        );
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn public_reset_clears_a_failed_population_refresh() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        assert!(
            hydration()
                .lock()
                .unwrap()
                .refresh_with(10, || Err("database unavailable".into()))
                .is_err()
        );
        super::super::reset_global_atc_state_for_test(&config);
        assert_eq!(
            atc_population_hydration_stats(),
            AtcPopulationHydrationStats::default()
        );
        assert_eq!(
            super::super::atc_tick_report(2_000_000)
                .unwrap()
                .summary
                .tick_count,
            1
        );
        super::super::reset_global_atc_state_for_test(&config);
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
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        hydration()
            .lock()
            .unwrap()
            .refresh_with(10, || Ok(rows(65)))
            .unwrap();
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
            assert_eq!(
                report.summary.tick_count, 0,
                "inference ran on a partial population"
            );
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
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        hydration()
            .lock()
            .unwrap()
            .refresh_with(10, || Ok(rows(65)))
            .unwrap();
        engine::atc_observe_activity_with_project(
            "HydratedAgent0064",
            Some("/hydration"),
            9_000_000,
        );
        while atc_population_hydration_stats().pending_agents > 0 {
            let _ = super::super::atc_tick_report(10_000_000).unwrap();
        }
        assert_eq!(
            engine::atc_agent_last_activity("HydratedAgent0064"),
            Some(9_000_000)
        );
        super::super::reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn public_reset_discards_old_hydration_and_unchanged_cache() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        hydration()
            .lock()
            .unwrap()
            .refresh_with(10, || Ok(rows(940)))
            .unwrap();
        super::super::reset_global_atc_state_for_test(&config);
        assert_eq!(
            atc_population_hydration_stats(),
            AtcPopulationHydrationStats::default()
        );
        assert!(hydration().lock().unwrap().previous_snapshot.is_empty());
        assert!(
            super::super::atc_tick_report(2_000_000)
                .unwrap()
                .summary
                .tracked_agents
                .is_empty()
        );
    }

    #[test]
    fn summary_does_not_wait_for_an_operator_population_query_lock() {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        super::super::reset_global_atc_state_for_test(&config);
        let _query_guard = hydration().lock().unwrap();
        let summary = super::super::atc_summary().unwrap();
        assert_eq!(summary.completeness, engine::SnapshotCompleteness::Partial);
        assert_eq!(
            summary.policy.fallback_reason.as_deref(),
            Some("population_hydration_incomplete")
        );
    }
}
