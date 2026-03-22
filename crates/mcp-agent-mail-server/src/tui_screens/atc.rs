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

use crate::atc::{
    AgentStateSnapshot, AtcDecisionRecord, AtcSummarySnapshot, LivenessState, atc_summary,
};
use crate::tui_bridge::TuiSharedState;
use crate::tui_screens::{HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_theme::TuiThemePalette;
use crate::tui_widgets::fancy::SummaryFooter;
use crate::tui_widgets::{MetricTile, MetricTrend};

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

// ── Screen state ─────────────────────────────────────────────────────

pub struct AtcScreen {
    /// Cached ATC summary snapshot.
    snapshot: Option<AtcSummarySnapshot>,
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
        self.snapshot = atc_summary();
        self.prev_decisions_total = prev_decisions;
        self.prev_agent_count = prev_agents;
    }

    fn sorted_agents(&self) -> Vec<&AgentStateSnapshot> {
        let Some(snap) = self.snapshot.as_ref() else {
            return Vec::new();
        };
        let mut agents: Vec<&AgentStateSnapshot> = snap.tracked_agents.iter().collect();
        agents.sort_by(|a, b| {
            let ord = match self.agent_sort_col {
                COL_STATE => format!("{:?}", a.state).cmp(&format!("{:?}", b.state)),
                COL_POSTERIOR => a
                    .posterior_alive
                    .partial_cmp(&b.posterior_alive)
                    .unwrap_or(std::cmp::Ordering::Equal),
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

    // ── Rendering helpers ────────────────────────────────────────────

    fn render_summary_tiles(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = TuiThemePalette::current();
        let Some(snap) = self.snapshot.as_ref() else {
            let p = Paragraph::new(" ATC engine not initialized — waiting for first tick...")
                .style(Style::default().fg(tp.text_muted));
            p.render(area, frame);
            return;
        };

        // 6 metric tiles in a horizontal strip
        let cols = Flex::horizontal().gap(1).constraints([
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Min(14),
        ]);
        let rects = cols.split(area);

        let decisions_trend = if snap.decisions_total > self.prev_decisions_total {
            MetricTrend::Up
        } else {
            MetricTrend::Flat
        };
        let agents_trend = if snap.tracked_agents.len() > self.prev_agent_count {
            MetricTrend::Up
        } else if snap.tracked_agents.len() < self.prev_agent_count {
            MetricTrend::Down
        } else {
            MetricTrend::Flat
        };

        MetricTile::new(
            "Decisions",
            &snap.decisions_total.to_string(),
            decisions_trend,
        )
        .value_color(tp.metric_requests)
        .render(rects[0], frame);
        MetricTile::new(
            "Agents",
            &snap.tracked_agents.len().to_string(),
            agents_trend,
        )
        .value_color(tp.metric_agents)
        .render(rects[1], frame);
        MetricTile::new(
            "Deadlocks",
            &snap.deadlock_cycles.to_string(),
            if snap.deadlock_cycles > 0 {
                MetricTrend::Up
            } else {
                MetricTrend::Flat
            },
        )
        .value_color(if snap.deadlock_cycles > 0 {
            tp.severity_error
        } else {
            tp.severity_ok
        })
        .render(rects[2], frame);

        let eprocess_label = format!("{:.3}", snap.eprocess_value);
        MetricTile::new("E-Process", &eprocess_label, MetricTrend::Flat)
            .value_color(tp.metric_latency)
            .render(rects[3], frame);

        let regret_label = format!("{:.2}", snap.regret_avg);
        MetricTile::new("Avg Regret", &regret_label, MetricTrend::Flat)
            .value_color(tp.metric_messages)
            .render(rects[4], frame);

        let mode_label = if !snap.enabled {
            "Disabled"
        } else if snap.safe_mode {
            "Safe Mode"
        } else {
            "Active"
        };
        let mode_color = if !snap.enabled {
            tp.text_disabled
        } else if snap.safe_mode {
            tp.severity_warn
        } else {
            tp.severity_ok
        };
        MetricTile::new("Mode", mode_label, MetricTrend::Flat)
            .value_color(mode_color)
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
                let state_str = match agent.state {
                    LivenessState::Alive => "Alive",
                    LivenessState::Flaky => "Flaky",
                    LivenessState::Dead => "Dead",
                };
                let state_color = match agent.state {
                    LivenessState::Alive => tp.severity_ok,
                    LivenessState::Flaky => tp.severity_warn,
                    LivenessState::Dead => tp.severity_error,
                };
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

        let snap = self.snapshot.as_ref();
        let decisions: Vec<&AtcDecisionRecord> = snap
            .map(|s| {
                s.recent_decisions
                    .iter()
                    .rev()
                    .take(MAX_VISIBLE_DECISIONS)
                    .collect()
            })
            .unwrap_or_default();

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

        let header = Row::new(vec!["#", "Subsys", "Subject", "Action", "E[Loss]", "Safe"])
            .style(Style::default().fg(tp.table_header_fg));

        let rows: Vec<Row> = decisions
            .iter()
            .enumerate()
            .map(|(idx, d)| {
                let loss_str = format!("{:.1}", d.expected_loss);
                let safe_str = if d.safe_mode_active { "Y" } else { "N" };
                let subsys_str = d.subsystem.to_string();
                let subsys_color = subsystem_color(&subsys_str, &tp);
                let row_bg = if idx % 2 == 0 {
                    tp.bg_deep
                } else {
                    tp.table_row_alt_bg
                };
                let subsys_line =
                    Line::from(Span::styled(subsys_str, Style::default().fg(subsys_color)));
                Row::new([
                    Line::raw(format!("{}", d.id)),
                    subsys_line,
                    Line::raw(truncate_str(&d.subject, 18)),
                    Line::raw(truncate_str(&d.action, 18)),
                    Line::raw(loss_str),
                    Line::raw(safe_str.to_string()),
                ])
                .style(Style::default().bg(row_bg))
            })
            .collect();

        let widths = [
            Constraint::Fixed(5),
            Constraint::Fixed(12),
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Fixed(8),
            Constraint::Fixed(5),
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
            .title(" Detail ")
            .border_type(BorderType::Rounded)
            .style(Style::default().fg(tp.panel_border));
        let inner = block.inner(area);
        block.render(area, frame);

        let Some(snap) = self.snapshot.as_ref() else {
            return;
        };

        let mut lines: Vec<String> = Vec::with_capacity(64);

        // -- Decision detail (if a decision is selected) --
        if self.focus == FocusPanel::Decisions {
            let decisions: Vec<&AtcDecisionRecord> = snap
                .recent_decisions
                .iter()
                .rev()
                .take(MAX_VISIBLE_DECISIONS)
                .collect();
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
                let state_str = match agent.state {
                    LivenessState::Alive => "Alive",
                    LivenessState::Flaky => "Flaky",
                    LivenessState::Dead => "Dead",
                };
                lines.push(format!("  State:       {state_str}"));
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
        lines.push("── Telemetry ──".to_string());
        lines.push(format!("  Tick count:     {}", snap.tick_count));

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
        lines.push(format!("  Policy:         {}", p.incumbent_policy_id));
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
        let items: &[(&str, &str, ftui::PackedRgba)] = &[
            (alive_str, "Alive", tp.severity_ok),
            (flaky_str, "Flaky", tp.severity_warn),
            (dead_str, "Dead", tp.severity_error),
            (decisions_str, "Decisions", tp.metric_requests),
            (ticks_str, "Ticks", tp.text_secondary),
        ];
        SummaryFooter::new(items, tp.text_muted).render(area, frame);
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
            KeyCode::Char('s') => {
                if self.focus == FocusPanel::Agents {
                    self.agent_sort_col = (self.agent_sort_col + 1) % AGENT_SORT_LABELS.len();
                }
            }
            KeyCode::Char('S') => {
                if self.focus == FocusPanel::Agents {
                    self.agent_sort_asc = !self.agent_sort_asc;
                }
            }
            // Detail toggle
            KeyCode::Char('i') => {
                self.detail_visible = !self.detail_visible;
            }
            // Detail scroll
            KeyCode::Char('J') => {
                if self.detail_visible {
                    self.detail_scroll = self.detail_scroll.saturating_add(3);
                }
            }
            KeyCode::Char('K') => {
                if self.detail_visible {
                    self.detail_scroll = self.detail_scroll.saturating_sub(3);
                }
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
                .filter(|a| matches!(a.state, LivenessState::Alive))
                .count()
        });
        let flaky_count = snap.map_or(0, |s| {
            s.tracked_agents
                .iter()
                .filter(|a| matches!(a.state, LivenessState::Flaky))
                .count()
        });
        let dead_count = snap.map_or(0, |s| {
            s.tracked_agents
                .iter()
                .filter(|a| matches!(a.state, LivenessState::Dead))
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
        let snap = self.snapshot.as_ref()?;
        match self.focus {
            FocusPanel::Agents => {
                let agents = self.sorted_agents();
                self.agent_table
                    .selected
                    .and_then(|idx| agents.get(idx))
                    .map(|a| {
                        format!(
                            "{} ({:?}) P(Alive)={:.1}%",
                            a.name,
                            a.state,
                            a.posterior_alive * 100.0
                        )
                    })
            }
            FocusPanel::Decisions => {
                let decisions: Vec<&AtcDecisionRecord> = snap
                    .recent_decisions
                    .iter()
                    .rev()
                    .take(MAX_VISIBLE_DECISIONS)
                    .collect();
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
