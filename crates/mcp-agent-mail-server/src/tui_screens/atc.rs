//! ATC (Air Traffic Controller) screen — decision engine dashboard with agent
//! liveness, conflict state, evidence ledger, risk budgets, calibration status,
//! and recent decision log.

use ftui::layout::{Breakpoint, Constraint, Flex, Rect, ResponsiveLayout};
use ftui::text::{Line, Span};
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, Style};
use ftui_runtime::program::Cmd;

use mcp_agent_mail_core::{LearningArtifactKind, retention_rule};

use crate::atc::AtcDecisionRecord;
use crate::tui_bridge::TuiSharedState;
use crate::tui_screens::{HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_theme::TuiThemePalette;
use crate::tui_widgets::{MetricTile, MetricTrend};
use crate::{
    AtcOperatorAgentSnapshot, AtcOperatorExecutionSnapshot, AtcOperatorSnapshot,
    atc_operator_snapshot,
};

// ── Constants ────────────────────────────────────────────────────────

/// How often to refresh ATC data (every N ticks = N*100ms at fast cadence).
const REFRESH_TICK_DIVISOR: u64 = 5;

/// Maximum decision records shown in the log table.
const MAX_VISIBLE_DECISIONS: usize = 64;

/// Agent table sort columns.
const COL_AGENT: usize = 0;
const COL_STATE: usize = 1;
const COL_POSTERIOR: usize = 2;
const COL_SILENCE: usize = 3;
const AGENT_SORT_LABELS: &[&str] = &["Agent", "State", "P(Alive)", "Silence"];

// ── Focus panels ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusPanel {
    Agents,
    Decisions,
}

impl FocusPanel {
    const fn next(self) -> Self {
        match self {
            Self::Agents => Self::Decisions,
            Self::Decisions => Self::Agents,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailMode {
    Selection,
    Retention,
}

// ── Screen state ─────────────────────────────────────────────────────

pub struct AtcScreen {
    /// Cached ATC summary snapshot.
    snapshot: Option<AtcOperatorSnapshot>,
    /// Agent table state.
    agent_table: TableState,
    /// Agent sort column.
    agent_sort_col: usize,
    /// Agent sort ascending.
    agent_sort_asc: bool,
    /// Decision log table state.
    decision_table: TableState,
    /// Which panel has focus.
    focus: FocusPanel,
    /// Detail panel visible.
    detail_visible: bool,
    /// Detail surface mode.
    detail_mode: DetailMode,
    /// Detail scroll offset.
    detail_scroll: usize,
    /// Previous metric values for trend arrows.
    prev_decisions_total: u64,
    prev_agent_count: usize,
    /// Last data generation for dirty-state tracking.
    _last_data_gen: super::DataGeneration,
    /// Tick counter for refresh cadence.
    tick_count: u64,
}

impl AtcScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            snapshot: None,
            agent_table: TableState::default(),
            agent_sort_col: COL_AGENT,
            agent_sort_asc: true,
            decision_table: TableState::default(),
            focus: FocusPanel::Agents,
            detail_visible: true,
            detail_mode: DetailMode::Selection,
            detail_scroll: 0,
            prev_decisions_total: 0,
            prev_agent_count: 0,
            _last_data_gen: super::DataGeneration::stale(),
            tick_count: 0,
        }
    }

    fn refresh_snapshot(&mut self) {
        let prev_decisions = self.snapshot.as_ref().map_or(0, |s| s.decisions_total);
        let prev_agents = self.snapshot.as_ref().map_or(0, |s| s.tracked_agents.len());
        self.snapshot = Some(atc_operator_snapshot());
        self.prev_decisions_total = prev_decisions;
        self.prev_agent_count = prev_agents;
        if let Some(snapshot) = self.snapshot.as_ref() {
            tracing::debug!(
                event = "tui.atc.snapshot_consumed",
                source = %snapshot.source,
                age_micros = snapshot_age_micros(snapshot).unwrap_or(0)
            );
        }
    }

    fn sorted_agents(&self) -> Vec<&AtcOperatorAgentSnapshot> {
        let Some(snap) = self.snapshot.as_ref() else {
            return Vec::new();
        };
        let mut agents: Vec<&AtcOperatorAgentSnapshot> = snap.tracked_agents.iter().collect();
        agents.sort_by(|a, b| {
            let ord = match self.agent_sort_col {
                COL_STATE => atc_agent_state_rank(a.state.as_str())
                    .cmp(&atc_agent_state_rank(b.state.as_str()))
                    .then_with(|| a.state.cmp(&b.state)),
                COL_POSTERIOR => a.posterior_alive.total_cmp(&b.posterior_alive),
                COL_SILENCE => a.silence_secs.cmp(&b.silence_secs),
                _ => a.name.cmp(&b.name),
            };
            if self.agent_sort_asc {
                ord
            } else {
                ord.reverse()
            }
        });
        agents
    }

    fn move_agent_selection(&mut self, delta: i32) {
        let count = self.snapshot.as_ref().map_or(0, |s| s.tracked_agents.len());
        if count == 0 {
            return;
        }
        let current = self.agent_table.selected.unwrap_or(0);
        #[allow(clippy::cast_sign_loss)]
        let next = if delta > 0 {
            current
                .saturating_add(delta as usize)
                .min(count.saturating_sub(1))
        } else {
            current.saturating_sub(delta.unsigned_abs() as usize)
        };
        self.agent_table.selected = Some(next);
    }

    fn move_decision_selection(&mut self, delta: i32) {
        let count = self
            .snapshot
            .as_ref()
            .map_or(0, |s| s.recent_decisions.len().min(MAX_VISIBLE_DECISIONS));
        if count == 0 {
            return;
        }
        let current = self.decision_table.selected.unwrap_or(0);
        #[allow(clippy::cast_sign_loss)]
        let next = if delta > 0 {
            current
                .saturating_add(delta as usize)
                .min(count.saturating_sub(1))
        } else {
            current.saturating_sub(delta.unsigned_abs() as usize)
        };
        self.decision_table.selected = Some(next);
    }

    fn visible_decisions(&self) -> Vec<&AtcDecisionRecord> {
        self.snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .recent_decisions
                    .iter()
                    .rev()
                    .take(MAX_VISIBLE_DECISIONS)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn decision_outcomes(&self) -> std::collections::BTreeMap<u64, String> {
        let mut outcomes = std::collections::BTreeMap::new();
        let Some(snapshot) = self.snapshot.as_ref() else {
            return outcomes;
        };
        let mut executions = snapshot.recent_executions.clone();
        executions.sort_by_key(|execution| execution.timestamp_micros);
        for execution in executions {
            outcomes.insert(execution.decision_id, execution_status_label(&execution));
        }
        outcomes
    }

    // ── Rendering helpers ────────────────────────────────────────────

    fn render_summary_tiles(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = TuiThemePalette::current();
        let Some(snap) = self.snapshot.as_ref() else {
            let p = Paragraph::new(" ATC engine not initialized — waiting for first tick...")
                .style(Style::default().fg(tp.text_muted));
            p.render(area, frame);
            return;
        };

        let cols = Flex::horizontal().gap(1).constraints([
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Min(14),
        ]);
        let rects = cols.split(area);

        MetricTile::new("ATC", atc_availability_label(snap), MetricTrend::Flat)
            .value_color(atc_availability_color(snap, &tp))
            .render(rects[0], frame);
        let policy_label = atc_policy_label(snap);
        MetricTile::new("Policy", &policy_label, MetricTrend::Flat)
            .value_color(tp.metric_messages)
            .render(rects[1], frame);
        MetricTile::new(
            "Safe",
            if snap.safe_mode { "ON" } else { "off" },
            MetricTrend::Flat,
        )
        .value_color(if snap.safe_mode {
            tp.severity_warn
        } else {
            tp.severity_ok
        })
        .render(rects[2], frame);
        let last_tick = format_timestamp_compact(snap.last_tick_micros);
        MetricTile::new("Last Tick", &last_tick, MetricTrend::Flat)
            .value_color(tp.metric_latency)
            .render(rects[3], frame);
        MetricTile::new(
            "Open",
            &snap.experiences_open.to_string(),
            if snap.experiences_open > 0 {
                MetricTrend::Up
            } else {
                MetricTrend::Flat
            },
        )
        .value_color(tp.metric_requests)
        .render(rects[4], frame);
        MetricTile::new(
            "Resolved",
            &snap.experiences_resolved.to_string(),
            if snap.experiences_resolved > 0 {
                MetricTrend::Up
            } else {
                MetricTrend::Flat
            },
        )
        .value_color(tp.metric_agents)
        .render(rects[5], frame);
    }

    fn render_agent_table(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = TuiThemePalette::current();
        let focused = self.focus == FocusPanel::Agents;
        let border_color = if focused {
            tp.panel_border_focused
        } else {
            tp.panel_border
        };

        let sort_indicator = if self.agent_sort_asc { " ▲" } else { " ▼" };
        let sort_label = AGENT_SORT_LABELS
            .get(self.agent_sort_col)
            .copied()
            .unwrap_or("?");
        let title = format!(" Tracked Agents [{sort_label}{sort_indicator}] ");

        let block = Block::default()
            .title(title.as_str())
            .border_type(BorderType::Rounded)
            .style(Style::default().fg(border_color));

        let agents = self.sorted_agents();

        if agents.is_empty() {
            let inner = block.inner(area);
            block.render(area, frame);
            let p =
                Paragraph::new(" No agents tracked yet").style(Style::default().fg(tp.text_muted));
            p.render(inner, frame);
            return;
        }

        let header = Row::new(vec!["Agent", "State", "P(Alive)", "Silence"])
            .style(Style::default().fg(tp.table_header_fg));

        let rows: Vec<Row> = agents
            .iter()
            .enumerate()
            .map(|(idx, agent)| {
                let state_str = agent.state.as_str();
                let state_color = atc_agent_state_color(state_str, &tp);
                let posterior_str = format!("{:.1}%", agent.posterior_alive * 100.0);
                let silence_str = format_silence(agent.silence_secs);
                let row_bg = if idx % 2 == 0 {
                    tp.bg_deep
                } else {
                    tp.table_row_alt_bg
                };
                let state_line = Line::from(Span::styled(
                    state_str.to_string(),
                    Style::default().fg(state_color),
                ));
                Row::new([
                    Line::raw(agent.name.clone()),
                    state_line,
                    Line::raw(posterior_str),
                    Line::raw(silence_str),
                ])
                .style(Style::default().bg(row_bg))
            })
            .collect();

        let widths = [
            Constraint::Min(16),
            Constraint::Fixed(8),
            Constraint::Fixed(10),
            Constraint::Fixed(10),
        ];

        let mut table_state = self.agent_table.clone();
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().bg(tp.selection_bg).fg(tp.selection_fg));
        <Table as StatefulWidget>::render(&table, area, frame, &mut table_state);
    }

    fn render_decision_log(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = TuiThemePalette::current();
        let focused = self.focus == FocusPanel::Decisions;
        let border_color = if focused {
            tp.panel_border_focused
        } else {
            tp.panel_border
        };

        let decisions = self.visible_decisions();
        let outcomes = self.decision_outcomes();

        let title = format!(" Evidence Ledger [{}] ", decisions.len());
        let block = Block::default()
            .title(title.as_str())
            .border_type(BorderType::Rounded)
            .style(Style::default().fg(border_color));

        if decisions.is_empty() {
            let inner = block.inner(area);
            block.render(area, frame);
            let p = Paragraph::new(" No decisions recorded yet")
                .style(Style::default().fg(tp.text_muted));
            p.render(inner, frame);
            return;
        }

        let header = Row::new(vec!["Time", "Decision", "Action", "Outcome", "E[Loss]"])
            .style(Style::default().fg(tp.table_header_fg));

        let rows: Vec<Row> = decisions
            .iter()
            .enumerate()
            .map(|(idx, d)| {
                let loss_str = format!("{:.1}", d.expected_loss);
                let decision_label = format!("{}/{}", d.subsystem, d.decision_class);
                let subsys_color = subsystem_color(&d.subsystem.to_string(), &tp);
                let outcome = outcomes
                    .get(&d.id)
                    .cloned()
                    .unwrap_or_else(|| "open".to_string());
                let row_bg = if idx % 2 == 0 {
                    tp.bg_deep
                } else {
                    tp.table_row_alt_bg
                };
                let decision_line = Line::from(Span::styled(
                    truncate_str(&decision_label, 22),
                    Style::default().fg(subsys_color),
                ));
                Row::new([
                    Line::raw(format_timestamp_compact(d.timestamp_micros)),
                    decision_line,
                    Line::raw(truncate_str(&d.action, 18)),
                    Line::raw(truncate_str(&outcome, 18)),
                    Line::raw(loss_str),
                ])
                .style(Style::default().bg(row_bg))
            })
            .collect();

        let widths = [
            Constraint::Fixed(19),
            Constraint::Min(22),
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Fixed(8),
        ];

        let mut table_state = self.decision_table.clone();
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().bg(tp.selection_bg).fg(tp.selection_fg));
        <Table as StatefulWidget>::render(&table, area, frame, &mut table_state);
    }

    #[allow(clippy::cast_precision_loss)] // u64 micros → f64 ms for display only
    fn render_detail_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = TuiThemePalette::current();
        let block = Block::default()
            .title(match self.detail_mode {
                DetailMode::Selection => " Detail ",
                DetailMode::Retention => " Retention Report ",
            })
            .border_type(BorderType::Rounded)
            .style(Style::default().fg(tp.panel_border));
        let inner = block.inner(area);
        block.render(area, frame);

        let Some(snap) = self.snapshot.as_ref() else {
            return;
        };

        let mut lines: Vec<String> = Vec::with_capacity(64);

        if self.detail_mode == DetailMode::Retention {
            lines.extend(retention_report_lines(snap));
        }
        // -- Decision detail (if a decision is selected) --
        else if self.focus == FocusPanel::Decisions {
            let decisions = self.visible_decisions();
            let outcomes = self.decision_outcomes();
            if let Some(&decision) = self
                .decision_table
                .selected
                .and_then(|idx| decisions.get(idx))
            {
                lines.push(format!("Decision #{}", decision.id));
                lines.push(format!("  Subsystem:  {}", decision.subsystem));
                lines.push(format!("  Class:      {}", decision.decision_class));
                lines.push(format!("  Subject:    {}", decision.subject));
                lines.push(format!("  Action:     {}", decision.action));
                lines.push(format!("  E[Loss]:    {:.3}", decision.expected_loss));
                lines.push(format!("  Runner-up:  {:.3}", decision.runner_up_loss));
                lines.push(format!(
                    "  Gap:        {:.3}",
                    decision.runner_up_loss - decision.expected_loss
                ));
                lines.push(format!(
                    "  Outcome:    {}",
                    outcomes
                        .get(&decision.id)
                        .cloned()
                        .unwrap_or_else(|| "open".to_string())
                ));
                lines.push(format!(
                    "  Calibrated: {}",
                    if decision.calibration_healthy {
                        "yes"
                    } else {
                        "NO"
                    }
                ));
                lines.push(format!(
                    "  Safe mode:  {}",
                    if decision.safe_mode_active {
                        "ACTIVE"
                    } else {
                        "off"
                    }
                ));
                if let Some(ref reason) = decision.fallback_reason {
                    lines.push(format!("  Fallback:   {reason}"));
                }
                if let Some(ref policy) = decision.policy_id {
                    lines.push(format!("  Policy:     {policy}"));
                }
                lines.push(String::new());
                lines.push("Posterior:".to_string());
                for (state, prob) in &decision.posterior {
                    let pct = prob * 100.0;
                    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                    let bar_len = (prob * 30.0).round().max(0.0) as usize;
                    let bar: String = "█".repeat(bar_len.min(30));
                    lines.push(format!("  {state:<10} {pct:>6.1}%  {bar}"));
                }
                lines.push(String::new());
                lines.push("Loss table:".to_string());
                for entry in &decision.loss_table {
                    let marker = if entry.action == decision.action {
                        "→"
                    } else {
                        " "
                    };
                    lines.push(format!(
                        "  {marker} {:<22} E[L]={:.3}",
                        entry.action, entry.expected_loss
                    ));
                }
                lines.push(String::new());
                lines.push("Evidence:".to_string());
                for line in decision.evidence_summary.lines() {
                    lines.push(format!("  {line}"));
                }
                lines.push(String::new());
                lines.push(format!("  Trace: {}", decision.trace_id));
                lines.push(format!("  Claim: {}", decision.claim_id));
            }
        }
        // -- Agent detail --
        else {
            let agents = self.sorted_agents();
            if let Some(agent) = self.agent_table.selected.and_then(|idx| agents.get(idx)) {
                lines.push(format!("Agent: {}", agent.name));
                lines.push(format!("  State:       {}", agent.state));
                lines.push(format!(
                    "  P(Alive):    {:.1}%",
                    agent.posterior_alive * 100.0
                ));
                lines.push(format!(
                    "  Silence:     {}",
                    format_silence(agent.silence_secs)
                ));
                lines.push(String::new());
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let bar_len = (agent.posterior_alive * 40.0).round().max(0.0) as usize;
                let bar: String = "█".repeat(bar_len.min(40));
                let empty: String = "░".repeat(40_usize.saturating_sub(bar_len));
                lines.push(format!(
                    "  [{bar}{empty}] {:.1}%",
                    agent.posterior_alive * 100.0
                ));
            }
        }

        // -- Telemetry section --
        lines.push(String::new());
        lines.push("── Snapshot Telemetry ──".to_string());
        lines.push(format!("  Source:         {}", snap.source));
        lines.push(format!(
            "  Last tick:      {}",
            format_timestamp_compact(snap.last_tick_micros)
        ));
        lines.push(format!("  Tick count:     {}", snap.tick_count));
        lines.push(format!("  Policy:         {}", atc_policy_label(snap)));
        lines.push(format!(
            "  Learning:       decisions={} open={} resolved={}",
            snap.decisions_total, snap.experiences_open, snap.experiences_resolved
        ));
        lines.push(format!(
            "  Fairness:       e-process={:.3} regret_avg={:.3}",
            snap.eprocess_value, snap.regret_avg
        ));
        lines.push(format!(
            "  Debt surface:   {}",
            top_open_strata_label(&snap.observability.experiences_open_by_stratum)
        ));

        // Stage timings
        let st = &snap.stage_timings;
        lines.push(format!(
            "  Stage timings:  liveness={:.1}ms deadlock={:.1}ms probe={:.1}ms",
            st.liveness_micros as f64 / 1000.0,
            st.deadlock_micros as f64 / 1000.0,
            st.probe_micros as f64 / 1000.0,
        ));
        lines.push(format!(
            "                  gating={:.1}ms slow_ctrl={:.1}ms summary={:.1}ms total={:.1}ms",
            st.gating_micros as f64 / 1000.0,
            st.slow_control_micros as f64 / 1000.0,
            st.summary_micros as f64 / 1000.0,
            st.total_micros as f64 / 1000.0,
        ));

        // Kernel telemetry
        let k = &snap.kernel;
        lines.push(format!(
            "  Kernel:         due={} scheduled={} dirty_agents={} dirty_proj={}",
            k.due_agents, k.scheduled_agents, k.dirty_agents, k.dirty_projects,
        ));
        lines.push(format!(
            "                  pending_fx={} lock_wait={:.1}ms dl_cache={:.0}%",
            k.pending_effects,
            k.lock_wait_micros as f64 / 1000.0,
            k.deadlock_cache_hit_rate * 100.0,
        ));

        // Budget telemetry
        let b = &snap.budget;
        lines.push(format!(
            "  Budget:         mode={} util={:.0}% slow_util={:.0}%",
            b.mode,
            b.utilization_ratio * 100.0,
            b.slow_window_utilization * 100.0,
        ));
        lines.push(format!(
            "                  tick={:.1}ms probe={:.1}ms max_probes={} debt={:.1}ms",
            b.tick_budget_micros as f64 / 1000.0,
            b.probe_budget_micros as f64 / 1000.0,
            b.max_probes_this_tick,
            b.budget_debt_micros as f64 / 1000.0,
        ));

        // Policy telemetry
        let p = &snap.policy;
        lines.push(format!("  Incumbent:      {}", p.incumbent_policy_id));
        lines.push(format!(
            "                  mode={} shadow={}",
            p.decision_mode,
            if p.shadow_enabled { "on" } else { "off" },
        ));
        if p.shadow_enabled {
            lines.push(format!(
                "                  shadow_disagree={} shadow_regret={:.3}",
                p.shadow_disagreements, p.shadow_regret_avg,
            ));
        }
        if p.fallback_active {
            lines.push(format!(
                "                  FALLBACK: {}",
                p.fallback_reason.as_deref().unwrap_or("unknown"),
            ));
        }

        // Apply scroll
        let visible_height = inner.height as usize;
        let scroll = self
            .detail_scroll
            .min(lines.len().saturating_sub(visible_height));
        let visible_lines: Vec<String> = lines
            .into_iter()
            .skip(scroll)
            .take(visible_height)
            .collect();
        let text = visible_lines.join("\n");
        let p = Paragraph::new(text).style(Style::default().fg(tp.text_primary));
        p.render(inner, frame);
    }

    fn render_summary_footer(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        alive_str: &str,
        flaky_str: &str,
        dead_str: &str,
        decisions_str: &str,
        ticks_str: &str,
    ) {
        let tp = TuiThemePalette::current();
        let footer = Line::from_spans([
            Span::styled(
                format!("Alive {alive_str}  "),
                Style::default().fg(tp.severity_ok),
            ),
            Span::styled(
                format!("Flaky {flaky_str}  "),
                Style::default().fg(tp.severity_warn),
            ),
            Span::styled(
                format!("Dead {dead_str}  "),
                Style::default().fg(tp.severity_error),
            ),
            Span::styled(
                format!("Decisions {decisions_str}  "),
                Style::default().fg(tp.metric_requests),
            ),
            Span::styled(
                format!("Ticks {ticks_str}  "),
                Style::default().fg(tp.text_secondary),
            ),
            Span::styled("d", Style::default().fg(tp.selection_fg)),
            Span::styled(" decision detail  ", Style::default().fg(tp.text_muted)),
            Span::styled("r", Style::default().fg(tp.selection_fg)),
            Span::styled(" retention report", Style::default().fg(tp.text_muted)),
        ]);
        Paragraph::new(footer).render(area, frame);
    }
}

// ── MailScreen implementation ────────────────────────────────────────

impl MailScreen for AtcScreen {
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        let Event::Key(key) = event else {
            return Cmd::None;
        };
        if key.kind != KeyEventKind::Press {
            return Cmd::None;
        }

        match key.code {
            // Panel switching
            KeyCode::Tab => {
                tracing::debug!(
                    event = "tui.atc.key_pressed",
                    key = "Tab",
                    action = "switch_panel"
                );
                self.focus = self.focus.next();
            }
            // Navigation
            KeyCode::Char('j') | KeyCode::Down => match self.focus {
                FocusPanel::Agents => self.move_agent_selection(1),
                FocusPanel::Decisions => self.move_decision_selection(1),
            },
            KeyCode::Char('k') | KeyCode::Up => match self.focus {
                FocusPanel::Agents => self.move_agent_selection(-1),
                FocusPanel::Decisions => self.move_decision_selection(-1),
            },
            KeyCode::Char('G') | KeyCode::End => match self.focus {
                FocusPanel::Agents => {
                    let count = self.snapshot.as_ref().map_or(0, |s| s.tracked_agents.len());
                    if count > 0 {
                        self.agent_table.selected = Some(count - 1);
                    }
                }
                FocusPanel::Decisions => {
                    let count = self
                        .snapshot
                        .as_ref()
                        .map_or(0, |s| s.recent_decisions.len().min(MAX_VISIBLE_DECISIONS));
                    if count > 0 {
                        self.decision_table.selected = Some(count - 1);
                    }
                }
            },
            KeyCode::Char('g') | KeyCode::Home => match self.focus {
                FocusPanel::Agents => self.agent_table.selected = Some(0),
                FocusPanel::Decisions => self.decision_table.selected = Some(0),
            },
            // Agent table sort
            KeyCode::Char('s') if self.focus == FocusPanel::Agents => {
                self.agent_sort_col = (self.agent_sort_col + 1) % AGENT_SORT_LABELS.len();
            }
            KeyCode::Char('S') if self.focus == FocusPanel::Agents => {
                self.agent_sort_asc = !self.agent_sort_asc;
            }
            // Detail toggle
            KeyCode::Char('i') => {
                tracing::debug!(
                    event = "tui.atc.key_pressed",
                    key = "i",
                    action = "toggle_detail"
                );
                self.detail_visible = !self.detail_visible;
                self.detail_mode = DetailMode::Selection;
            }
            // Detail scroll
            KeyCode::Char('J') if self.detail_visible => {
                self.detail_scroll = self.detail_scroll.saturating_add(3);
            }
            KeyCode::Char('K') if self.detail_visible => {
                self.detail_scroll = self.detail_scroll.saturating_sub(3);
            }
            KeyCode::Char('d') => {
                tracing::debug!(
                    event = "tui.atc.key_pressed",
                    key = "d",
                    action = "decision_detail"
                );
                self.detail_visible = true;
                self.detail_mode = DetailMode::Selection;
                self.focus = FocusPanel::Decisions;
                if self.decision_table.selected.is_none() && !self.visible_decisions().is_empty() {
                    self.decision_table.selected = Some(0);
                }
                if let Some(decision) = self
                    .decision_table
                    .selected
                    .and_then(|idx| self.visible_decisions().get(idx).copied())
                {
                    tracing::debug!(
                        event = "tui.atc.drill_in",
                        decision_id = decision.id,
                        trace_id = %decision.trace_id
                    );
                }
            }
            KeyCode::Char('r') => {
                tracing::debug!(
                    event = "tui.atc.key_pressed",
                    key = "r",
                    action = "retention_report"
                );
                self.detail_visible = true;
                self.detail_mode = DetailMode::Retention;
            }
            _ => {}
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, _state: &TuiSharedState) {
        self.tick_count = tick_count;
        if tick_count.is_multiple_of(REFRESH_TICK_DIVISOR) {
            self.refresh_snapshot();
        }
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        tracing::debug!(
            event = "tui.atc.render_start",
            viewport = format!("{}x{}", area.width, area.height),
            theme = crate::tui_theme::current_theme_name()
        );
        if area.width < 40 || area.height < 10 {
            let p = Paragraph::new(" Terminal too small for ATC screen");
            p.render(area, frame);
            return;
        }

        // Vertical layout: tiles | main panels | footer
        let vertical = Flex::vertical().constraints([
            Constraint::Fixed(3), // summary tiles
            Constraint::Min(10),  // main content
            Constraint::Fixed(1), // summary footer
        ]);
        let vsplit = vertical.split(area);

        self.render_summary_tiles(frame, vsplit[0]);

        // Pre-compute footer strings so they outlive the render call.
        let snap = self.snapshot.as_ref();
        let alive_count = snap.map_or(0, |s| {
            s.tracked_agents
                .iter()
                .filter(|a| a.state.eq_ignore_ascii_case("alive"))
                .count()
        });
        let flaky_count = snap.map_or(0, |s| {
            s.tracked_agents
                .iter()
                .filter(|a| a.state.eq_ignore_ascii_case("flaky"))
                .count()
        });
        let dead_count = snap.map_or(0, |s| {
            s.tracked_agents
                .iter()
                .filter(|a| a.state.eq_ignore_ascii_case("dead"))
                .count()
        });
        let decisions_total = snap.map_or(0, |s| s.decisions_total);
        let tick_count = snap.map_or(0, |s| s.tick_count);
        let alive_str = alive_count.to_string();
        let flaky_str = flaky_count.to_string();
        let dead_str = dead_count.to_string();
        let decisions_str = decisions_total.to_string();
        let ticks_str = tick_count.to_string();
        self.render_summary_footer(
            frame,
            vsplit[2],
            &alive_str,
            &flaky_str,
            &dead_str,
            &decisions_str,
            &ticks_str,
        );

        // Main content: tables (left) + optional detail (right)
        if self.detail_visible && area.width >= 100 {
            let layout = ResponsiveLayout::new(
                Flex::vertical()
                    .constraints([Constraint::Percentage(50.0), Constraint::Percentage(50.0)]),
            )
            .at(
                Breakpoint::Lg,
                Flex::horizontal()
                    .constraints([Constraint::Percentage(55.0), Constraint::Percentage(45.0)]),
            )
            .at(
                Breakpoint::Xl,
                Flex::horizontal()
                    .constraints([Constraint::Percentage(60.0), Constraint::Percentage(40.0)]),
            );
            let split = layout.split(vsplit[1]);
            self.render_tables_panel(frame, split.rects[0]);
            if split.rects.len() >= 2 {
                self.render_detail_panel(frame, split.rects[1]);
            }
        } else {
            self.render_tables_panel(frame, vsplit[1]);
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Navigate list",
            },
            HelpEntry {
                key: "g/G",
                action: "Jump to first/last",
            },
            HelpEntry {
                key: "Tab",
                action: "Switch panel (Agents/Decisions)",
            },
            HelpEntry {
                key: "s",
                action: "Cycle sort column",
            },
            HelpEntry {
                key: "S",
                action: "Toggle sort direction",
            },
            HelpEntry {
                key: "i",
                action: "Toggle detail panel",
            },
            HelpEntry {
                key: "d",
                action: "Drill into decision detail",
            },
            HelpEntry {
                key: "r",
                action: "Open retention report",
            },
            HelpEntry {
                key: "J/K",
                action: "Scroll detail panel",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("ATC decision engine: agent liveness, conflict detection, and evidence ledger")
    }

    fn title(&self) -> &'static str {
        "ATC"
    }

    fn tab_label(&self) -> &'static str {
        "ATC"
    }

    fn copyable_content(&self) -> Option<String> {
        let _ = self.snapshot.as_ref()?;
        match self.focus {
            FocusPanel::Agents => {
                let agents = self.sorted_agents();
                self.agent_table
                    .selected
                    .and_then(|idx| agents.get(idx))
                    .map(|a| {
                        format!(
                            "{} ({}) P(Alive)={:.1}%",
                            a.name,
                            a.state,
                            a.posterior_alive * 100.0
                        )
                    })
            }
            FocusPanel::Decisions => {
                let decisions = self.visible_decisions();
                self.decision_table
                    .selected
                    .and_then(|idx| decisions.get(idx))
                    .map(|d| d.format_message())
            }
        }
    }
}

impl AtcScreen {
    /// Render the agent table and decision log stacked vertically.
    fn render_tables_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let split = Flex::vertical()
            .constraints([Constraint::Percentage(40.0), Constraint::Percentage(60.0)])
            .split(area);
        self.render_agent_table(frame, split[0]);
        self.render_decision_log(frame, split[1]);
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn format_silence(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn subsystem_color(subsystem: &str, tp: &TuiThemePalette) -> ftui::PackedRgba {
    match subsystem {
        "liveness" => tp.severity_ok,
        "conflict" => tp.severity_warn,
        "load_routing" => tp.metric_latency,
        "synthesis" => tp.metric_messages,
        "calibration" => tp.metric_requests,
        _ => tp.text_secondary,
    }
}

fn atc_agent_state_rank(state: &str) -> u8 {
    if state.eq_ignore_ascii_case("dead") {
        2
    } else if state.eq_ignore_ascii_case("flaky") {
        1
    } else {
        0
    }
}

fn atc_agent_state_color(state: &str, tp: &TuiThemePalette) -> ftui::PackedRgba {
    if state.eq_ignore_ascii_case("dead") {
        tp.severity_error
    } else if state.eq_ignore_ascii_case("flaky") {
        tp.severity_warn
    } else {
        tp.severity_ok
    }
}

fn atc_availability_label(snapshot: &AtcOperatorSnapshot) -> &'static str {
    if !snapshot.enabled {
        "Disabled"
    } else if snapshot.source == "warming_up" {
        "Warmup"
    } else if snapshot.source == "spawn_failed" {
        "Failed"
    } else if snapshot.source == "live" {
        "Live"
    } else {
        "Fallback"
    }
}

fn atc_availability_color(
    snapshot: &AtcOperatorSnapshot,
    tp: &TuiThemePalette,
) -> ftui::PackedRgba {
    if !snapshot.enabled {
        tp.text_disabled
    } else if snapshot.source == "spawn_failed" {
        tp.severity_error
    } else if snapshot.source == "live" {
        tp.severity_ok
    } else {
        tp.severity_warn
    }
}

fn atc_policy_label(snapshot: &AtcOperatorSnapshot) -> String {
    if !snapshot.policy.bundle_id.is_empty() {
        snapshot.policy.bundle_id.clone()
    } else {
        format!("rev-{}", snapshot.policy_revision)
    }
}

fn format_timestamp_compact(timestamp_micros: i64) -> String {
    if timestamp_micros <= 0 {
        return "--".to_string();
    }
    let iso = mcp_agent_mail_db::micros_to_iso(timestamp_micros);
    iso.replace('T', " ").chars().take(19).collect()
}

fn snapshot_age_micros(snapshot: &AtcOperatorSnapshot) -> Option<u64> {
    (snapshot.last_tick_micros > 0)
        .then(|| mcp_agent_mail_db::now_micros().saturating_sub(snapshot.last_tick_micros) as u64)
}

fn execution_status_label(execution: &AtcOperatorExecutionSnapshot) -> String {
    execution.status_detail.as_deref().map_or_else(
        || execution.status.clone(),
        |detail| format!("{}:{detail}", execution.status),
    )
}

fn top_open_strata_label(open_by_stratum: &std::collections::BTreeMap<String, u64>) -> String {
    let mut strata: Vec<_> = open_by_stratum.iter().collect();
    strata.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
    if strata.is_empty() {
        "none".to_string()
    } else {
        strata
            .into_iter()
            .take(3)
            .map(|(stratum, count)| format!("{stratum}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn retention_report_lines(snapshot: &AtcOperatorSnapshot) -> Vec<String> {
    let rollup = &snapshot.observability.rollup_refresh_latency_micros;
    let mut lines = vec![
        "ATC retention and rollup policy".to_string(),
        String::new(),
        format!(
            "  Last tick:       {}",
            format_timestamp_compact(snapshot.last_tick_micros)
        ),
        format!(
            "  Rollup refresh:  count={} p95={}us",
            rollup.count, rollup.p95
        ),
        format!(
            "  Rows deleted:    {}",
            snapshot.observability.retention_rows_deleted_total
        ),
        format!(
            "  Open strata:     {}",
            top_open_strata_label(&snapshot.observability.experiences_open_by_stratum)
        ),
        String::new(),
        "Canonical retention rules:".to_string(),
    ];
    for kind in [
        LearningArtifactKind::OpenExperienceRows,
        LearningArtifactKind::ResolvedExperienceRows,
        LearningArtifactKind::ExperienceRollups,
        LearningArtifactKind::EvidenceLedgerEntries,
    ] {
        if let Some(rule) = retention_rule(kind) {
            let archive = if matches!(
                rule.archive_retention,
                mcp_agent_mail_core::ArchiveRetention::Never
            ) {
                "never".to_string()
            } else if let Some(days) = rule.archive_retention.minimum_days() {
                format!("min {days}d")
            } else {
                "indefinite".to_string()
            };
            lines.push(format!(
                "  {kind:?}: hot={}d compact_after={:?} drop_after={:?} archive={} story={}",
                rule.hot_days,
                rule.compact_after_days,
                rule.drop_after_days,
                archive,
                rule.operator_story
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ftui::Frame;
    use ftui_harness::buffer_to_text;

    fn sample_snapshot() -> AtcOperatorSnapshot {
        AtcOperatorSnapshot {
            enabled: true,
            source: "live".to_string(),
            safe_mode: true,
            kill_switch_enabled: true,
            tick_count: 42,
            experiences_open: 3,
            experiences_resolved: 9,
            policy_revision: 7,
            tracked_agents: vec![
                AtcOperatorAgentSnapshot {
                    name: "AlphaAgent".to_string(),
                    state: "alive".to_string(),
                    silence_secs: 9,
                    posterior_alive: 0.98,
                },
                AtcOperatorAgentSnapshot {
                    name: "BetaAgent".to_string(),
                    state: "dead".to_string(),
                    silence_secs: 480,
                    posterior_alive: 0.02,
                },
            ],
            deadlock_cycles: 1,
            eprocess_value: 1.23,
            regret_avg: 0.42,
            decisions_total: 11,
            observability: mcp_agent_mail_core::metrics::AtcMetricsSnapshot {
                experiences_open_by_stratum: std::collections::BTreeMap::from([
                    ("liveness:probe:0".to_string(), 2),
                    ("conflict:release:2".to_string(), 1),
                ]),
                rollup_refresh_latency_micros: mcp_agent_mail_core::metrics::HistogramSnapshot {
                    count: 3,
                    sum: 42_000,
                    min: 10_000,
                    max: 20_000,
                    p50: 12_000,
                    p95: 18_000,
                    p99: 20_000,
                },
                retention_rows_deleted_total: 5,
                ..Default::default()
            },
            recent_decisions: vec![AtcDecisionRecord {
                id: 17,
                claim_id: "atc-claim-17".to_string(),
                evidence_id: "atc-evidence-17".to_string(),
                trace_id: "atc-trace-17".to_string(),
                timestamp_micros: 1_700_000_000_000_000,
                subsystem: crate::atc::AtcSubsystem::Liveness,
                decision_class: "probe_schedule".to_string(),
                subject: "BetaAgent".to_string(),
                policy_id: Some("bundle-r7".to_string()),
                posterior: vec![("dead".to_string(), 0.91)],
                action: "Probe".to_string(),
                expected_loss: 1.2,
                runner_up_loss: 2.4,
                loss_table: vec![crate::atc::AtcLossTableEntry {
                    action: "Probe".to_string(),
                    expected_loss: 1.2,
                }],
                evidence_summary: "beta silent".to_string(),
                calibration_healthy: true,
                safe_mode_active: true,
                fallback_reason: Some("budget_pressure".to_string()),
            }],
            recent_executions: vec![AtcOperatorExecutionSnapshot {
                timestamp_micros: 1_700_000_000_500_000,
                decision_id: 17,
                experience_id: Some(99),
                effect_id: "atc-effect-17".to_string(),
                claim_id: "atc-claim-17".to_string(),
                evidence_id: "atc-evidence-17".to_string(),
                trace_id: "atc-trace-17".to_string(),
                kind: "probe_agent".to_string(),
                category: "liveness".to_string(),
                agent: "BetaAgent".to_string(),
                project_key: Some("/tmp/project".to_string()),
                policy_id: Some("bundle-r7".to_string()),
                policy_revision: 7,
                execution_mode: "shadow".to_string(),
                status: "failed".to_string(),
                status_detail: Some("timeout".to_string()),
                message: Some("probe timed out".to_string()),
            }],
            last_tick_micros: 1_700_000_000_900_000,
            last_tick_duration_micros: 90_000,
            last_tick_budget_micros: 120_000,
            executor_mode: "shadow".to_string(),
            budget: crate::atc::AtcBudgetTelemetry {
                mode: "guarded".to_string(),
                budget_debt_micros: 8_000,
                ..Default::default()
            },
            policy: crate::atc::AtcPolicyTelemetry {
                bundle_id: "bundle-r7".to_string(),
                incumbent_policy_id: "policy/incumbent".to_string(),
                decision_mode: "shadow".to_string(),
                shadow_enabled: true,
                shadow_disagreements: 2,
                shadow_regret_avg: 0.3,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn keybindings_include_decision_and_retention_shortcuts() {
        let screen = AtcScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.iter().any(|binding| binding.key == "d"));
        assert!(bindings.iter().any(|binding| binding.key == "r"));
    }

    #[test]
    fn decision_log_renders_timestamp_and_outcome() {
        let mut screen = AtcScreen::new();
        screen.snapshot = Some(sample_snapshot());
        screen.focus = FocusPanel::Decisions;
        screen.decision_table.selected = Some(0);

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(160, 12, &mut pool);
        screen.render_decision_log(&mut frame, Rect::new(0, 0, 160, 12));

        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("Outcome"),
            "expected outcome column, got:\n{text}"
        );
        assert!(
            text.contains("failed:timeout"),
            "expected execution outcome in decision table, got:\n{text}"
        );
        assert!(
            text.contains("liveness/probe"),
            "expected decision class in table, got:\n{text}"
        );
    }

    #[test]
    fn retention_detail_renders_rollup_and_rules() {
        let mut screen = AtcScreen::new();
        screen.snapshot = Some(sample_snapshot());
        screen.detail_mode = DetailMode::Retention;
        screen.detail_visible = true;

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 18, &mut pool);
        screen.render_detail_panel(&mut frame, Rect::new(0, 0, 120, 18));

        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("Canonical retention rules"),
            "expected retention rule section, got:\n{text}"
        );
        assert!(
            text.contains("Rollup refresh"),
            "expected rollup refresh summary, got:\n{text}"
        );
        assert!(
            text.contains("OpenExperienceRows") || text.contains("open_experience_rows"),
            "expected named learning artifact in retention report, got:\n{text}"
        );
    }
}
