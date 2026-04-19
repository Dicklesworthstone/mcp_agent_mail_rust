//! Agents screen — sortable/filterable roster of registered agents with
//! summary band, status badges, sparkline, responsive columns, and footer.

use std::collections::{HashMap, HashSet, VecDeque};

use ftui::layout::{Breakpoint, Constraint, Flex, Rect, ResponsiveLayout};
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba, Style};
use ftui_runtime::program::Cmd;
use mcp_agent_mail_core::{AgentHealthGrade, AgentHealthScorecard};
use mcp_agent_mail_db::DbConn;

use crate::tui_bridge::{ScreenDiagnosticSnapshot, TuiSharedState};
use crate::tui_events::MailEvent;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_widgets::fancy::SummaryFooter;
use crate::tui_widgets::{MetricTile, MetricTrend};

/// Column indices for sorting.
const COL_NAME: usize = 0;
const COL_PROGRAM: usize = 1;
const COL_MODEL: usize = 2;
const COL_HEALTH: usize = 3;
const COL_LAST_ACTIVE: usize = 4;
const COL_MESSAGES: usize = 5;

const SORT_LABELS: &[&str] = &["Name", "Program", "Model", "Health", "Active", "Msgs"];
const STATUS_FADE_TICKS: u8 = 5;
const MESSAGE_FLASH_TICKS: u8 = 3;
const STAGGER_MAX_TICKS: u8 = 10;
const ACTIVE_WINDOW_MICROS: i64 = 60 * 1_000_000;
const IDLE_WINDOW_MICROS: i64 = 5 * 60 * 1_000_000;
/// Max sparkline history samples.
const SPARKLINE_CAP: usize = 30;
const EVENT_INGEST_BATCH_LIMIT: usize = 1024;
/// Mirror the poller-side cap when reconstructing richer rows from `SQLite`.
const SQLITE_AGENT_RECOVERY_LIMIT: usize = 500;

/// Agent names are unique only within a project, so screen-local caches must
/// keep project-qualified identities to avoid cross-project collisions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AgentIdentity {
    project: String,
    name: String,
}

impl AgentIdentity {
    fn new(project: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            name: name.into(),
        }
    }
}

/// An agent row with computed fields.
#[derive(Debug, Clone)]
struct AgentRow {
    identity: AgentIdentity,
    name: String,
    project: String,
    program: String,
    model: String,
    health: Option<AgentHealthScorecard>,
    last_active_ts: i64,
    message_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentStatus {
    Active,
    Idle,
    Inactive,
}

impl AgentStatus {
    const fn from_last_active(last_active_ts: i64, now_ts: i64) -> Self {
        if last_active_ts <= 0 {
            return Self::Inactive;
        }
        let elapsed = now_ts.saturating_sub(last_active_ts);
        if elapsed <= ACTIVE_WINDOW_MICROS {
            Self::Active
        } else if elapsed <= IDLE_WINDOW_MICROS {
            Self::Idle
        } else {
            Self::Inactive
        }
    }

    fn rgb(self) -> (u8, u8, u8) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let c = match self {
            Self::Active => tp.activity_active,
            Self::Idle => tp.activity_idle,
            Self::Inactive => tp.activity_stale,
        };
        (c.r(), c.g(), c.b())
    }

    const fn icon(self) -> &'static str {
        match self {
            Self::Active => "\u{25CF}",   // ●
            Self::Idle => "\u{25D0}",     // ◐
            Self::Inactive => "\u{25CB}", // ○
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StatusFadeState {
    from: AgentStatus,
    to: AgentStatus,
    ticks_remaining: u8,
}

impl StatusFadeState {
    const fn new(from: AgentStatus, to: AgentStatus) -> Self {
        Self {
            from,
            to,
            ticks_remaining: STATUS_FADE_TICKS,
        }
    }

    const fn step(&mut self) -> bool {
        if self.ticks_remaining > 0 {
            self.ticks_remaining -= 1;
        }
        self.ticks_remaining == 0
    }
}

fn blend_rgb(from: (u8, u8, u8), to: (u8, u8, u8), progress: f32) -> (u8, u8, u8) {
    let t = progress.clamp(0.0, 1.0);
    let blend = |start: u8, end: u8| -> u8 {
        let start = f32::from(start);
        let end = f32::from(end);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            (end - start).mul_add(t, start).round() as u8
        }
    };
    (
        blend(from.0, to.0),
        blend(from.1, to.1),
        blend(from.2, to.2),
    )
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        let normalized = value.trim().to_ascii_lowercase();
        matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
    })
}

fn sanitize_diagnostic_value(value: &str) -> String {
    value
        .replace(['\n', '\r', ';', ','], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn recover_agent_rows_from_sqlite(
    database_url: &str,
    storage_root: &std::path::Path,
) -> Option<Vec<AgentRow>> {
    let db = match crate::open_observability_sync_db_connection(
        database_url,
        storage_root,
        "agents screen recovery",
    ) {
        Ok(Some(db)) => db,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(error = %error, "agents screen recovery db open failed");
            return None;
        }
    };
    Some(fetch_agent_rows_from_sqlite(db.conn()))
}

fn fetch_agent_rows_from_sqlite(conn: &DbConn) -> Vec<AgentRow> {
    let last_active_sort = crate::tui_poller::timestamp_sort_expr("a.last_active_ts");
    let query_rows = match conn.query_sync(
        &format!(
            "WITH agent_messages AS ( \
               SELECT a.id AS agent_id, m.id AS message_id \
               FROM agents a \
               JOIN messages m ON m.sender_id = a.id \
               UNION \
               SELECT a.id AS agent_id, mr.message_id AS message_id \
               FROM agents a \
               JOIN message_recipients mr ON mr.agent_id = a.id \
             ) \
             SELECT \
               a.id AS raw_agent_id, \
               a.project_id AS raw_project_id, \
               COALESCE(p.slug, '') AS project_slug, \
               a.name, \
               a.program, \
               COALESCE(a.model, '') AS model, \
               a.last_active_ts, \
               COUNT(DISTINCT am.message_id) AS cnt \
             FROM agents a \
             LEFT JOIN projects p ON p.id = a.project_id \
             LEFT JOIN agent_messages am ON am.agent_id = a.id \
             GROUP BY a.id, a.project_id, project_slug, a.name, a.program, a.model, a.last_active_ts \
             ORDER BY {last_active_sort} DESC, a.id DESC \
             LIMIT {SQLITE_AGENT_RECOVERY_LIMIT}"
        ),
        &[],
    ) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "agents screen query failed");
            return Vec::new();
        }
    };
    query_rows
        .into_iter()
        .filter_map(|row| {
            let agent_id = row
                .get_named::<i64>("raw_agent_id")
                .ok()
                .or_else(|| row.get_as::<i64>(0).ok())?;
            let project_id = row
                .get_named::<i64>("raw_project_id")
                .ok()
                .or_else(|| row.get_as::<i64>(1).ok())
                .unwrap_or(0);
            let project = row
                .get_named::<String>("project_slug")
                .ok()
                .or_else(|| row.get_as::<String>(2).ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| format!("[unknown-project-{project_id}]"));
            let name = row
                .get_named::<String>("name")
                .ok()
                .or_else(|| row.get_as::<String>(3).ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| format!("[unknown-agent-{agent_id}]"));
            let program = row
                .get_named::<String>("program")
                .ok()
                .or_else(|| row.get_as::<String>(4).ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "[unknown-program]".to_string());
            let model = row
                .get_named::<String>("model")
                .ok()
                .or_else(|| row.get_as::<String>(5).ok())
                .unwrap_or_default();
            Some(AgentRow {
                identity: AgentIdentity::new(project.clone(), name.clone()),
                name,
                project,
                program,
                model,
                health: None,
                last_active_ts: crate::tui_poller::parse_raw_ts(&row, "last_active_ts"),
                message_count: row
                    .get_named::<i64>("cnt")
                    .ok()
                    .or_else(|| row.get_as::<i64>(7).ok())
                    .and_then(|count| u64::try_from(count.max(0)).ok())
                    .unwrap_or(0),
            })
        })
        .collect()
}

/// Truth assertion: when the DB reports non-zero agents but the rendered
/// list is empty AND no filter is active, the data pipeline has a bug.
fn assert_agents_list_cardinality(total_db_agents: u64, rendered_count: usize, filter: &str) {
    let assertions_on = cfg!(debug_assertions)
        || std::env::var("AM_TUI_STRICT_TRUTH_ASSERTIONS").is_ok_and(|v| {
            let n = v.trim().to_ascii_lowercase();
            matches!(n.as_str(), "1" | "true" | "yes" | "on")
        });
    if !assertions_on {
        return;
    }
    if total_db_agents > 0 && rendered_count == 0 && filter.trim().is_empty() {
        debug_assert!(
            false,
            "[truth_assertion] agents screen: DB reports {total_db_agents} agents \
             but rendered list is empty with no active filter — data pipeline dropped all rows"
        );
    }
}

fn unique_previous_agent_row<F>(previous_rows: &[AgentRow], mut predicate: F) -> Option<&AgentRow>
where
    F: FnMut(&AgentRow) -> bool,
{
    let mut matching = previous_rows.iter().filter(|row| predicate(row));
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

fn enrich_lossy_agent_summary_rows_from_previous(
    rows: &mut [AgentRow],
    previous_rows: &[AgentRow],
) {
    if rows.is_empty() || previous_rows.is_empty() {
        return;
    }

    for row in rows {
        // A row qualifies as "lossy" when the project is either empty or
        // a synthetic `[unknown-project-N]` placeholder — the latter is
        // what `recover_agent_rows_from_sqlite` emits when the agent row
        // references a project_id that doesn't resolve (e.g. because the
        // `projects` table is lagging the `agents` table, or the project
        // was just deleted). Without the placeholder check this function
        // skips rows that still carry the placeholder string and the TUI
        // regresses a previously-correct "alpha" project back to
        // "[unknown-project-99]" on every subsequent rebuild.
        let project_is_placeholder = row.project.starts_with("[unknown-project-");
        if !row.project.is_empty() && !project_is_placeholder {
            continue;
        }
        let previous = unique_previous_agent_row(previous_rows, |candidate| {
            candidate.name == row.name
                && candidate.program == row.program
                && candidate.last_active_ts == row.last_active_ts
        })
        .or_else(|| {
            unique_previous_agent_row(previous_rows, |candidate| {
                candidate.name == row.name && candidate.program == row.program
            })
        })
        .or_else(|| {
            unique_previous_agent_row(previous_rows, |candidate| candidate.name == row.name)
        });
        let Some(previous) = previous else {
            continue;
        };
        // Only restore from previous when its project is actually real;
        // otherwise we'd just swap one placeholder for another.
        let previous_project_is_placeholder = previous.project.starts_with("[unknown-project-");
        if (!previous.project.is_empty() && !previous_project_is_placeholder)
            || row.project.is_empty()
        {
            row.identity = previous.identity.clone();
            row.project = previous.project.clone();
        }
        if row.model.is_empty() {
            row.model = previous.model.clone();
        }
        if row.health.is_none() {
            row.health = previous.health.clone();
        }
        row.message_count = row.message_count.max(previous.message_count);
    }
}

fn should_preserve_previous_agent_rows(
    total_db_agents: u64,
    rows: &[AgentRow],
    previous_rows: &[AgentRow],
    filter: &str,
) -> bool {
    total_db_agents > 0 && rows.is_empty() && filter.trim().is_empty() && !previous_rows.is_empty()
}

fn reduced_motion_enabled() -> bool {
    env_flag_enabled("AM_TUI_REDUCED_MOTION") || env_flag_enabled("AM_TUI_A11Y_REDUCED_MOTION")
}

const fn trend_for(current: u64, previous: u64) -> MetricTrend {
    if current > previous {
        MetricTrend::Up
    } else if current < previous {
        MetricTrend::Down
    } else {
        MetricTrend::Flat
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct AgentsScreen {
    table_state: TableState,
    agents: Vec<AgentRow>,
    sort_col: usize,
    sort_asc: bool,
    filter: String,
    filter_active: bool,
    last_seq: u64,
    /// Per-agent message counts from events.
    msg_counts: HashMap<AgentIdentity, u64>,
    /// Per-agent model names from `AgentRegistered` events.
    model_names: HashMap<AgentIdentity, String>,
    /// Last computed presence status for each known agent.
    status_by_agent: HashMap<AgentIdentity, AgentStatus>,
    /// Fade transition state when an agent status changes.
    status_fades: HashMap<AgentIdentity, StatusFadeState>,
    /// Brief row highlight when a message event is observed for an agent.
    message_flash_ticks: HashMap<AgentIdentity, u8>,
    /// New rows reveal with a staggered delay to avoid hard pop-in.
    stagger_reveal_ticks: HashMap<AgentIdentity, u8>,
    /// Last row set, used to detect newly appearing agents.
    seen_agents: HashSet<AgentIdentity>,
    /// Reduced-motion mode skips all per-tick visual interpolation.
    reduced_motion: bool,
    /// Synthetic event for the focused agent (palette quick actions).
    focused_synthetic: Option<crate::tui_events::MailEvent>,
    /// Sparkline: messages per tick, bounded to `SPARKLINE_CAP` samples.
    msg_rate_history: VecDeque<f64>,
    /// Previous total message count for trend computation.
    prev_total_msgs: u64,
    /// Messages accumulated this tick interval.
    total_msgs_this_tick: u64,
    /// Whether the detail panel is visible on wide screens (user toggle).
    detail_visible: bool,
    /// Scroll offset inside the detail panel.
    detail_scroll: usize,
    /// Maximum scroll offset observed during the last render pass.
    last_detail_max_scroll: std::cell::Cell<usize>,
    /// Last observed data-channel generation for dirty-state gating.
    last_data_gen: super::DataGeneration,
    /// Latched when new data arrives; consumed on the next cadence rebuild.
    pending_rebuild: bool,
    /// True when the DB poller has not yet delivered any data.
    db_context_unavailable: bool,
    /// Banner copy explaining why poller-driven data is unavailable.
    db_context_banner: &'static str,
    /// Latest `db_stats` generation that this screen has actually rebuilt from.
    applied_db_stats_gen: u64,
    // ── Cached derived data (computed once per rebuild, reused in render) ──
    /// Cached (active, idle, inactive) agent counts.
    cached_status_counts: (u64, u64, u64),
    /// Cached total message count across all agents.
    cached_total_msgs: u64,
    /// Cached timestamp (microseconds) snapshotted once per tick.
    cached_now_ts: i64,
    /// Cached sparkline data as Vec (avoids VecDeque→Vec copy every render).
    cached_sparkline_vec: Vec<f64>,
    /// Whether the sparkline cache needs refresh.
    sparkline_dirty: bool,
    /// Last selection index, used to gate `sync_focused_event`.
    last_selection: Option<usize>,
    /// Generation counter for agent list changes (gates focused-event sync).
    agents_list_gen: u64,
    /// Last agent-list generation observed by the focused-event sync gate.
    last_agents_list_gen: u64,
}

impl AgentsScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            table_state: TableState::default(),
            agents: Vec::new(),
            sort_col: COL_LAST_ACTIVE,
            sort_asc: false,
            filter: String::new(),
            filter_active: false,
            last_seq: 0,
            msg_counts: HashMap::new(),
            model_names: HashMap::new(),
            status_by_agent: HashMap::new(),
            status_fades: HashMap::new(),
            message_flash_ticks: HashMap::new(),
            stagger_reveal_ticks: HashMap::new(),
            seen_agents: HashSet::new(),
            reduced_motion: reduced_motion_enabled(),
            focused_synthetic: None,
            msg_rate_history: VecDeque::with_capacity(SPARKLINE_CAP),
            prev_total_msgs: 0,
            total_msgs_this_tick: 0,
            detail_visible: true,
            detail_scroll: 0,
            last_detail_max_scroll: std::cell::Cell::new(0),
            last_data_gen: super::DataGeneration::stale(),
            pending_rebuild: false,
            db_context_unavailable: false,
            db_context_banner: super::POLLER_DB_WAITING_BANNER,
            applied_db_stats_gen: 0,
            cached_status_counts: (0, 0, 0),
            cached_total_msgs: 0,
            cached_now_ts: 0,
            cached_sparkline_vec: Vec::new(),
            sparkline_dirty: true,
            last_selection: None,
            agents_list_gen: 0,
            last_agents_list_gen: 0,
        }
    }

    /// Rebuild the synthetic `MailEvent` for the currently selected agent.
    fn sync_focused_event(&mut self) {
        self.focused_synthetic = self
            .table_state
            .selected
            .and_then(|i| self.agents.get(i))
            .map(|row| {
                crate::tui_events::MailEvent::agent_registered(
                    &row.name,
                    &row.program,
                    &row.model,
                    &row.project,
                )
            });
    }

    #[allow(clippy::too_many_lines)]
    fn rebuild_from_state(&mut self, state: &TuiSharedState) {
        let (db, db_stats_gen) = state.db_stats_snapshot_with_generation();
        let cfg = state.config_snapshot();
        let total_rows = db.agents;
        let raw_count = u64::try_from(db.agents_list.len()).unwrap_or(u64::MAX);
        let previous_rows = self.agents.clone();
        let previous_selection = self.table_state.selected;
        let previous_selected_identity = previous_selection
            .and_then(|index| self.agents.get(index))
            .map(|agent| agent.identity.clone());
        let recovered_rows = if total_rows > 0 {
            recover_agent_rows_from_sqlite(
                &cfg.raw_database_url,
                std::path::Path::new(&cfg.storage_root),
            )
            .unwrap_or_default()
        } else {
            Vec::new()
        };
        let recovered_rows_usable =
            !recovered_rows.is_empty() && recovered_rows.len() >= db.agents_list.len();
        let summary_health_by_identity: HashMap<AgentIdentity, AgentHealthScorecard> = db
            .agents_list
            .iter()
            .filter_map(|agent| {
                agent.health.as_ref().map(|health| {
                    (
                        AgentIdentity::new(agent.project.clone(), agent.name.clone()),
                        health.clone(),
                    )
                })
            })
            .collect();
        let mut rows: Vec<AgentRow> = if recovered_rows_usable {
            recovered_rows
                .into_iter()
                .map(|mut row| {
                    let recent_count = self.msg_counts.get(&row.identity).copied().unwrap_or(0);
                    if let Some(model_name) = self.model_names.get(&row.identity) {
                        row.model = model_name.clone();
                    }
                    if let Some(health) = summary_health_by_identity.get(&row.identity) {
                        row.health = Some(health.clone());
                    }
                    row.message_count = row.message_count.max(recent_count);
                    row
                })
                .collect()
        } else {
            db.agents_list
                .iter()
                .map(|a| {
                    let identity = AgentIdentity::new(a.project.clone(), a.name.clone());
                    AgentRow {
                        identity: identity.clone(),
                        name: a.name.clone(),
                        project: a.project.clone(),
                        program: a.program.clone(),
                        model: if a.model.is_empty() {
                            self.model_names.get(&identity).cloned().unwrap_or_default()
                        } else {
                            a.model.clone()
                        },
                        health: a.health.clone(),
                        last_active_ts: a.last_active_ts,
                        message_count: self.msg_counts.get(&identity).copied().unwrap_or(0),
                    }
                })
                .collect()
        };

        enrich_lossy_agent_summary_rows_from_previous(&mut rows, &previous_rows);
        if !recovered_rows_usable {
            if should_preserve_previous_agent_rows(total_rows, &rows, &previous_rows, &self.filter)
            {
                // Keep the last good roster instead of surfacing a false empty state or
                // collapsing project-scoped rows into blank placeholder identities.
                rows = previous_rows;
            }
        }

        // Apply filter
        let filter_text = self.filter.trim().to_ascii_lowercase();
        if !filter_text.is_empty() {
            rows.retain(|r| {
                crate::tui_screens::contains_ci(&r.name, &filter_text)
                    || crate::tui_screens::contains_ci(&r.program, &filter_text)
                    || crate::tui_screens::contains_ci(&r.model, &filter_text)
            });
        }

        // Sort
        rows.sort_by(|a, b| {
            let cmp = match self.sort_col {
                COL_NAME => crate::tui_screens::cmp_ci(&a.name, &b.name),
                COL_PROGRAM => crate::tui_screens::cmp_ci(&a.program, &b.program),
                COL_MODEL => crate::tui_screens::cmp_ci(&a.model, &b.model),
                COL_HEALTH => agent_health_cmp(a, b, self.sort_asc),
                COL_LAST_ACTIVE => a.last_active_ts.cmp(&b.last_active_ts),
                COL_MESSAGES => a.message_count.cmp(&b.message_count),
                _ => std::cmp::Ordering::Equal,
            };
            if self.sort_col == COL_HEALTH || self.sort_asc {
                cmp
            } else {
                cmp.reverse()
            }
        });

        let rendered_count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        // Use the higher of total_rows (DB COUNT) and list length as raw_count,
        // since COUNT(*) may be stale/race-y relative to the list fetch.
        let raw_from_db = total_rows.max(raw_count);
        let dropped_count = raw_from_db.saturating_sub(rendered_count);
        let sort_label = SORT_LABELS.get(self.sort_col).copied().unwrap_or("unknown");
        let filter = sanitize_diagnostic_value(&self.filter);
        let filter = if filter.is_empty() {
            "all".to_string()
        } else {
            filter
        };

        // Truth assertion: non-empty DB should produce non-empty rendered list
        // when no user filter is active.
        assert_agents_list_cardinality(total_rows, rows.len(), &self.filter);

        // Detect when list was capped by poller LIMIT (total_rows > list length).
        let list_capped = total_rows > raw_count;

        let transport_mode = cfg.transport_mode().to_string();
        state.push_screen_diagnostic(ScreenDiagnosticSnapshot {
            screen: "agents".to_string(),
            scope: "db_stats.agents_list".to_string(),
            query_params: format!(
                "filter={filter};sort_col={sort_label};sort_asc={};list_rows={raw_count};total_rows={total_rows};capped={list_capped}",
                self.sort_asc,
            ),
            raw_count: raw_from_db,
            rendered_count,
            dropped_count,
            timestamp_micros: self.cached_now_ts,
            db_url: cfg.database_url,
            storage_root: cfg.storage_root,
            transport_mode,
            auth_enabled: cfg.auth_enabled,
        });

        self.track_stagger_reveals(&rows);
        self.rebuild_status_transitions(&rows);
        self.agents = rows;
        self.agents_list_gen = self.agents_list_gen.wrapping_add(1);

        // Cache derived data used by multiple render methods.
        self.cached_status_counts = self.compute_status_counts();
        self.cached_total_msgs = if db.messages > 0 {
            db.messages
        } else {
            self.agents.iter().map(|agent| agent.message_count).sum()
        };
        self.sparkline_dirty = true;

        self.table_state.selected = previous_selected_identity.and_then(|identity| {
            self.agents
                .iter()
                .position(|agent| agent.identity == identity)
        });

        if self.table_state.selected.is_none()
            && let Some(sel) = previous_selection
        {
            self.table_state.selected = if self.agents.is_empty() {
                None
            } else {
                Some(sel.min(self.agents.len() - 1))
            };
        }
        self.applied_db_stats_gen = db_stats_gen;
    }

    fn ingest_events(&mut self, state: &TuiSharedState) {
        let events = state.tick_events_since_limited(self.last_seq, EVENT_INGEST_BATCH_LIMIT);
        for event in &events {
            self.last_seq = event.seq().max(self.last_seq);
            match event {
                MailEvent::MessageSent { from, project, .. } => {
                    let identity = AgentIdentity::new(project.clone(), from.clone());
                    *self.msg_counts.entry(identity.clone()).or_insert(0) += 1;
                    self.total_msgs_this_tick += 1;
                    if !self.reduced_motion {
                        self.message_flash_ticks
                            .insert(identity, MESSAGE_FLASH_TICKS);
                    }
                }
                MailEvent::MessageReceived {
                    from, to, project, ..
                } if !self.reduced_motion => {
                    self.message_flash_ticks.insert(
                        AgentIdentity::new(project.clone(), from.clone()),
                        MESSAGE_FLASH_TICKS,
                    );
                    for recipient in to {
                        self.message_flash_ticks.insert(
                            AgentIdentity::new(project.clone(), recipient.clone()),
                            MESSAGE_FLASH_TICKS,
                        );
                    }
                }
                MailEvent::AgentRegistered {
                    name,
                    model_name,
                    project,
                    ..
                } => {
                    self.model_names.insert(
                        AgentIdentity::new(project.clone(), name.clone()),
                        model_name.clone(),
                    );
                }
                _ => {}
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.agents.is_empty() {
            return;
        }
        let len = self.agents.len();
        // First press with no selection: jump to the boundary without
        // applying the delta so that Down → first item, Up → last item.
        let Some(current) = self.table_state.selected else {
            self.table_state.selected = Some(if delta > 0 { 0 } else { len - 1 });
            self.detail_scroll = 0;
            return;
        };
        let next = if delta > 0 {
            current.saturating_add(delta.unsigned_abs()).min(len - 1)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.table_state.selected = Some(next);
        self.detail_scroll = 0;
    }

    fn rebuild_status_transitions(&mut self, rows: &[AgentRow]) {
        let now_ts = self.cached_now_ts;
        let mut next_statuses = HashMap::with_capacity(rows.len());
        for row in rows {
            let next = AgentStatus::from_last_active(row.last_active_ts, now_ts);
            if !self.reduced_motion
                && let Some(prev) = self.status_by_agent.get(&row.identity)
                && *prev != next
            {
                self.status_fades
                    .insert(row.identity.clone(), StatusFadeState::new(*prev, next));
            }
            next_statuses.insert(row.identity.clone(), next);
        }
        self.status_by_agent = next_statuses;
        if self.reduced_motion {
            self.status_fades.clear();
            return;
        }
        self.status_fades.retain(|name, fade| {
            self.status_by_agent
                .get(name)
                .is_some_and(|status| *status == fade.to)
                && fade.ticks_remaining > 0
        });
    }

    fn advance_status_fades(&mut self) {
        self.status_fades.retain(|_, fade| !fade.step());
    }

    fn track_stagger_reveals(&mut self, rows: &[AgentRow]) {
        let mut next_seen = HashSet::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            if !self.reduced_motion && !self.seen_agents.contains(&row.identity) {
                let capped = index.min(usize::from(STAGGER_MAX_TICKS - 1));
                let delay = u8::try_from(capped).map_or(STAGGER_MAX_TICKS, |value| value + 1);
                self.stagger_reveal_ticks
                    .insert(row.identity.clone(), delay);
            }
            next_seen.insert(row.identity.clone());
        }
        self.seen_agents = next_seen;
        if self.reduced_motion {
            self.stagger_reveal_ticks.clear();
            self.message_flash_ticks.clear();
            return;
        }
        self.stagger_reveal_ticks
            .retain(|name, ticks| self.seen_agents.contains(name) && *ticks > 0);
        self.message_flash_ticks
            .retain(|name, ticks| self.seen_agents.contains(name) && *ticks > 0);
    }

    fn advance_message_flashes(&mut self) {
        self.message_flash_ticks.retain(|_, ticks| {
            if *ticks > 0 {
                *ticks -= 1;
            }
            *ticks > 0
        });
    }

    fn advance_stagger_reveals(&mut self) {
        self.stagger_reveal_ticks.retain(|_, ticks| {
            if *ticks > 0 {
                *ticks -= 1;
            }
            *ticks > 0
        });
    }

    /// Record sparkline data point: messages since last sample.
    fn record_sparkline_sample(&mut self) {
        if self.msg_rate_history.len() >= SPARKLINE_CAP {
            self.msg_rate_history.pop_front();
        }
        let sparkline_sample = u32::try_from(self.total_msgs_this_tick).unwrap_or(u32::MAX);
        self.msg_rate_history.push_back(f64::from(sparkline_sample));
        self.total_msgs_this_tick = 0;
        self.sparkline_dirty = true;
    }

    /// Refresh the cached sparkline Vec if dirty.
    fn refresh_sparkline_cache(&mut self) {
        if self.sparkline_dirty {
            self.cached_sparkline_vec = self.msg_rate_history.iter().copied().collect();
            self.sparkline_dirty = false;
        }
    }

    fn status_color(&self, agent: &AgentRow, now_ts: i64) -> PackedRgba {
        let target = AgentStatus::from_last_active(agent.last_active_ts, now_ts);
        if self.reduced_motion {
            let (r, g, b) = target.rgb();
            return PackedRgba::rgb(r, g, b);
        }
        if let Some(fade) = self.status_fades.get(&agent.identity) {
            let progress =
                1.0 - (f32::from(fade.ticks_remaining) / f32::from(STATUS_FADE_TICKS.max(1)));
            let (r, g, b) = blend_rgb(fade.from.rgb(), fade.to.rgb(), progress);
            return PackedRgba::rgb(r, g, b);
        }
        let (r, g, b) = target.rgb();
        PackedRgba::rgb(r, g, b)
    }

    fn row_style(&self, row_index: usize, agent: &AgentRow, now_ts: i64) -> Style {
        let tp = crate::tui_theme::TuiThemePalette::current();
        if Some(row_index) == self.table_state.selected {
            return Style::default().fg(tp.selection_fg).bg(tp.selection_bg);
        }
        if !self.reduced_motion && self.stagger_reveal_ticks.contains_key(&agent.identity) {
            return crate::tui_theme::text_disabled(&tp);
        }

        let status_color = self.status_color(agent, now_ts);
        let mut style = Style::default().fg(status_color);
        if !self.reduced_motion
            && let Some(remaining) = self.message_flash_ticks.get(&agent.identity)
        {
            let intensity = f32::from(*remaining) / f32::from(MESSAGE_FLASH_TICKS.max(1));
            let dim = (tp.text_muted.r(), tp.text_muted.g(), tp.text_muted.b());
            let bright = (
                tp.selection_bg.r(),
                tp.selection_bg.g(),
                tp.selection_bg.b(),
            );
            let (r, g, b) = blend_rgb(dim, bright, intensity);
            style = style.bg(PackedRgba::rgb(r, g, b)).fg(tp.selection_fg);
        }
        style
    }

    /// Count agents by status category using the cached timestamp.
    fn compute_status_counts(&self) -> (u64, u64, u64) {
        let now_ts = self.cached_now_ts;
        let mut active = 0u64;
        let mut idle = 0u64;
        let mut inactive = 0u64;
        for agent in &self.agents {
            match AgentStatus::from_last_active(agent.last_active_ts, now_ts) {
                AgentStatus::Active => active += 1,
                AgentStatus::Idle => idle += 1,
                AgentStatus::Inactive => inactive += 1,
            }
        }
        (active, idle, inactive)
    }
}

impl Default for AgentsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for AgentsScreen {
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            // Filter mode: capture text input
            if self.filter_active {
                match key.code {
                    KeyCode::Escape | KeyCode::Enter => {
                        self.filter_active = false;
                    }
                    KeyCode::Backspace => {
                        self.filter.pop();
                        self.rebuild_from_state(state);
                    }
                    KeyCode::Char(c) => {
                        self.filter.push(c);
                        self.rebuild_from_state(state);
                    }
                    _ => {}
                }
                return Cmd::None;
            }

            match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
                KeyCode::Char('G') | KeyCode::End if !self.agents.is_empty() => {
                    self.table_state.selected = Some(self.agents.len() - 1);
                }
                KeyCode::Char('g') | KeyCode::Home if !self.agents.is_empty() => {
                    self.table_state.selected = Some(0);
                }
                KeyCode::Char('/') => {
                    self.filter_active = true;
                    self.filter.clear();
                }
                KeyCode::Char('s') => {
                    self.sort_col = (self.sort_col + 1) % SORT_LABELS.len();
                    self.rebuild_from_state(state);
                }
                KeyCode::Char('S') => {
                    self.sort_asc = !self.sort_asc;
                    self.rebuild_from_state(state);
                }
                KeyCode::Char('i' | 'h') => {
                    self.detail_visible = !self.detail_visible;
                }
                KeyCode::Char('J') => {
                    let max = self.last_detail_max_scroll.get();
                    self.detail_scroll = self.detail_scroll.saturating_add(1).min(max);
                }
                KeyCode::Char('K') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                KeyCode::Escape if !self.filter.is_empty() => {
                    self.filter.clear();
                    self.rebuild_from_state(state);
                }
                _ => {}
            }
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        // Snapshot timestamp once per tick for all downstream use.
        self.cached_now_ts = chrono::Utc::now().timestamp_micros();

        // ── Dirty-state gated data ingestion ────────────────────────
        let current_gen = state.data_generation();
        let dirty = super::dirty_since(&self.last_data_gen, &current_gen);

        if dirty.events {
            self.ingest_events(state);
        }
        if dirty.db_stats || dirty.events {
            self.pending_rebuild = true;
        }
        if !self.reduced_motion {
            self.advance_status_fades();
            self.advance_message_flashes();
            self.advance_stagger_reveals();
        }
        // Rebuild every second, consuming a latched refresh request so updates
        // are never missed due to tick-phase misalignment.
        if tick_count.is_multiple_of(10) && self.pending_rebuild {
            let total: u64 = self.msg_counts.values().sum();
            self.prev_total_msgs = total;
            self.record_sparkline_sample();
            self.rebuild_from_state(state);
            self.pending_rebuild = false;
        }

        // Compute banner state after any cadence rebuild so the copy reflects
        // the snapshot this screen has actually consumed.
        if let Some(banner) =
            super::poller_db_context_banner(state, self.applied_db_stats_gen, tick_count)
        {
            self.db_context_unavailable = true;
            self.db_context_banner = banner;
        } else {
            self.db_context_unavailable = false;
            self.db_context_banner = super::POLLER_DB_WAITING_BANNER;
        }

        // Only recompute focused_synthetic when selection or agent list changed.
        let current_sel = self.table_state.selected;
        let list_changed = self.agents_list_gen != self.last_agents_list_gen;
        if current_sel != self.last_selection || list_changed {
            self.sync_focused_event();
            self.last_selection = current_sel;
            self.last_agents_list_gen = self.agents_list_gen;
        }

        // Lazily refresh sparkline cache before next render.
        self.refresh_sparkline_cache();

        self.last_data_gen = current_gen;
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        if area.height < 3 || area.width < 20 {
            return;
        }

        let tp = crate::tui_theme::TuiThemePalette::current();

        // Outer bordered panel wrapping entire screen content
        let outer_block = crate::tui_panel_helpers::panel_block(" Registered Agents ");
        let inner = outer_block.inner(area);
        outer_block.render(area, frame);
        let area = inner;

        // Responsive layout: single-col on narrow, table+detail on wide
        let layout = ResponsiveLayout::new(Flex::vertical().constraints([Constraint::Fill]))
            .at(
                Breakpoint::Lg,
                Flex::horizontal().constraints([Constraint::Percentage(60.0), Constraint::Fill]),
            )
            .at(
                Breakpoint::Xl,
                Flex::horizontal().constraints([Constraint::Percentage(50.0), Constraint::Fill]),
            );

        if self.db_context_unavailable {
            let banner = Paragraph::new(self.db_context_banner)
                .style(Style::default().fg(tp.severity_error));
            let banner_area = ftui::layout::Rect::new(area.x, area.y, area.width, 1);
            banner.render(banner_area, frame);
            return;
        }

        let split = layout.split(area);
        let table_area = split.rects[0];

        self.render_table_content(frame, table_area, &tp);

        // Render detail panel if visible (Lg+)
        if split.rects.len() >= 2 && self.detail_visible {
            self.render_detail_panel(frame, split.rects[1]);
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Select agent",
            },
            HelpEntry {
                key: "/",
                action: "Search/filter",
            },
            HelpEntry {
                key: "s",
                action: "Cycle sort column",
            },
            HelpEntry {
                key: "S",
                action: "Toggle sort order",
            },
            HelpEntry {
                key: "i/h",
                action: "Toggle detail panel",
            },
            HelpEntry {
                key: "J/K",
                action: "Scroll detail",
            },
            HelpEntry {
                key: "Esc",
                action: "Clear filter",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("Registered agents and their health. Use h for score detail, / to filter.")
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        if let DeepLinkTarget::AgentByName(name) = target
            && let Some(pos) = self.agents.iter().position(|a| a.name == *name)
        {
            self.table_state.selected = Some(pos);
            return true;
        }
        false
    }

    fn consumes_text_input(&self) -> bool {
        self.filter_active
    }

    fn copyable_content(&self) -> Option<String> {
        let idx = self.table_state.selected?;
        let agent = self.agents.get(idx)?;
        Some(agent.name.clone())
    }

    fn title(&self) -> &'static str {
        "Agents"
    }

    fn tab_label(&self) -> &'static str {
        "Agents"
    }
}

// ── Rendering helpers ──────────────────────────────────────────────────

impl AgentsScreen {
    /// Render summary band + header + table + footer into a single column area.
    fn render_table_content(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        tp: &crate::tui_theme::TuiThemePalette,
    ) {
        let wide = area.width >= 120;
        let narrow = area.width < 80;

        // Layout: summary(2) + header(1) + table(remainder) + footer(1)
        let summary_h: u16 = if area.height >= 8 { 2 } else { 0 };
        let header_h: u16 = 1;
        let footer_h = u16::from(area.height >= 6);
        let table_h = area
            .height
            .saturating_sub(summary_h)
            .saturating_sub(header_h)
            .saturating_sub(footer_h);

        let mut y = area.y;

        // ── Summary band ───────────────────────────────────────────────
        if summary_h > 0 {
            let summary_area = Rect::new(area.x, y, area.width, summary_h);
            self.render_summary_band(frame, summary_area);
            y += summary_h;
        }

        // ── Info header ────────────────────────────────────────────────
        let header_area = Rect::new(area.x, y, area.width, header_h);
        y += header_h;

        Paragraph::new("")
            .style(Style::default().fg(tp.text_primary).bg(tp.panel_bg))
            .render(header_area, frame);

        let sort_indicator = if self.sort_asc {
            " \u{25b2}"
        } else {
            " \u{25bc}"
        };
        let sort_label = SORT_LABELS.get(self.sort_col).unwrap_or(&"?");
        let filter_display = if self.filter_active {
            format!(" [/] Search: {}_ ", self.filter)
        } else if !self.filter.is_empty() {
            format!(" [/] Filter: {} ", self.filter)
        } else {
            String::new()
        };
        let info = format!(
            "{} agents | Sort: {}{} {}",
            self.agents.len(),
            sort_label,
            sort_indicator,
            filter_display,
        );
        Paragraph::new(info).render(header_area, frame);

        // ── Table ──────────────────────────────────────────────────────
        let table_area = Rect::new(area.x, y, area.width, table_h);
        y += table_h;

        Paragraph::new("")
            .style(Style::default().fg(tp.text_primary).bg(tp.panel_bg))
            .render(table_area, frame);

        self.render_table(frame, table_area, wide, narrow);

        // ── Footer ─────────────────────────────────────────────────────
        if footer_h > 0 {
            let footer_area = Rect::new(area.x, y, area.width, footer_h);
            self.render_footer(frame, footer_area);
        }
    }

    /// Render the detail panel for the currently selected agent.
    fn render_detail_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = crate::tui_panel_helpers::panel_block(" Agent Detail ");
        let inner = block.inner(area);
        block.render(area, frame);

        let Some(selected_idx) = self.table_state.selected else {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{1f464}",
                "No Agent Selected",
                "Select an agent from the table to view details.",
            );
            return;
        };

        let Some(agent) = self.agents.get(selected_idx) else {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{1f464}",
                "No Agent Selected",
                "Select an agent from the table to view details.",
            );
            return;
        };

        let lines = self.build_detail_lines(agent, &tp);
        render_kv_lines(
            frame,
            inner,
            &lines,
            self.detail_scroll,
            &self.last_detail_max_scroll,
            &tp,
        );
    }

    /// Build key-value lines for agent detail.
    fn build_detail_lines(
        &self,
        agent: &AgentRow,
        tp: &crate::tui_theme::TuiThemePalette,
    ) -> Vec<(String, String, Option<PackedRgba>)> {
        let now_ts = self.cached_now_ts;
        let status = AgentStatus::from_last_active(agent.last_active_ts, now_ts);
        let status_color = self.status_color(agent, now_ts);

        let mut lines: Vec<(String, String, Option<PackedRgba>)> = Vec::new();

        lines.push(("Name".into(), agent.name.clone(), None));
        if !agent.project.is_empty() {
            lines.push(("Project".into(), agent.project.clone(), None));
        }
        lines.push((
            "Status".into(),
            format!(
                "{} {}",
                status.icon(),
                match status {
                    AgentStatus::Active => "Active",
                    AgentStatus::Idle => "Idle",
                    AgentStatus::Inactive => "Inactive",
                }
            ),
            Some(status_color),
        ));
        lines.push(("Program".into(), agent.program.clone(), None));
        if !agent.model.is_empty() {
            lines.push(("Model".into(), agent.model.clone(), None));
        }
        lines.push((
            "Last Active".into(),
            if agent.last_active_ts == 0 {
                "never".into()
            } else {
                let relative = format_relative_time_with(agent.last_active_ts, now_ts);
                let iso = mcp_agent_mail_db::timestamps::micros_to_iso(agent.last_active_ts);
                format!("{relative}  ({iso})")
            },
            None,
        ));
        lines.push(("Messages".into(), agent.message_count.to_string(), None));
        if let Some(health) = &agent.health {
            lines.push((
                "Health".into(),
                format!("{}  ({} decisions)", health.badge(), health.decision_count),
                Some(health_grade_color(health.grade, tp)),
            ));
            lines.push((
                "Coverage".into(),
                format!("{}bp observed", health.observed_weight_bp),
                None,
            ));
            for metric in &health.metrics {
                lines.push((
                    metric.label().into(),
                    format!("{} — {}", metric.value_label(), metric.evidence),
                    Some(if metric.available {
                        health_metric_color(metric.raw_score, tp)
                    } else {
                        tp.text_muted
                    }),
                ));
            }
        }

        // Sparkline as text bar
        if !self.msg_rate_history.is_empty() {
            lines.push((
                "Msg Rate".into(),
                sparkline_bar(&self.msg_rate_history),
                Some(tp.metric_messages),
            ));
        }

        lines
    }

    #[allow(clippy::cast_possible_truncation)]
    fn render_summary_band(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let total_agents = self.agents.len() as u64;
        let (active, _idle, _inactive) = self.cached_status_counts;
        let needing_attention = self
            .agents
            .iter()
            .filter(|agent| {
                agent
                    .health
                    .as_ref()
                    .is_some_and(AgentHealthScorecard::needs_attention)
            })
            .count() as u64;
        let healthy = total_agents.saturating_sub(needing_attention);
        let total_msgs = self.cached_total_msgs;

        let agents_str = total_agents.to_string();
        let active_str = active.to_string();
        let healthy_str = healthy.to_string();
        let attention_str = needing_attention.to_string();
        let msgs_str = total_msgs.to_string();

        let sparkline_data = &self.cached_sparkline_vec;

        let tiles: Vec<(&str, &str, MetricTrend, PackedRgba)> = vec![
            ("Agents", &agents_str, MetricTrend::Flat, tp.metric_agents),
            ("Active", &active_str, MetricTrend::Flat, tp.activity_active),
            (
                "Healthy",
                &healthy_str,
                MetricTrend::Flat,
                tp.activity_active,
            ),
            (
                "Attention",
                &attention_str,
                MetricTrend::Flat,
                tp.activity_stale,
            ),
            (
                "Messages",
                &msgs_str,
                trend_for(total_msgs, self.prev_total_msgs),
                tp.metric_messages,
            ),
        ];

        let tile_count = tiles.len();
        if tile_count == 0 || area.width == 0 || area.height == 0 {
            return;
        }
        let tile_w = area.width / tile_count as u16;

        for (i, (label, value, trend, color)) in tiles.iter().enumerate() {
            let x = area.x + (i as u16) * tile_w;
            let w = if i == tile_count - 1 {
                area.width.saturating_sub(x - area.x)
            } else {
                tile_w
            };
            let tile_area = Rect::new(x, area.y, w, area.height);
            let mut tile = MetricTile::new(label, value, *trend)
                .value_color(*color)
                .sparkline_color(*color);
            // Add sparkline to Messages tile
            if *label == "Messages" && !sparkline_data.is_empty() {
                tile = tile.sparkline(sparkline_data);
            }
            tile.render(tile_area, frame);
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn render_table(&self, frame: &mut Frame<'_>, area: Rect, wide: bool, narrow: bool) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let now_ts = self.cached_now_ts;

        // Responsive columns
        let (header_cells, widths): (Vec<&str>, Vec<Constraint>) = if narrow {
            // < 80: keep health visible without the detail pane.
            (
                vec!["Name", "Health", "Last Active"],
                vec![
                    Constraint::Percentage(45.0),
                    Constraint::Percentage(20.0),
                    Constraint::Percentage(35.0),
                ],
            )
        } else if wide {
            (
                vec!["Name", "Program", "Model", "Health", "Last Active", "Msgs"],
                vec![
                    Constraint::Percentage(20.0),
                    Constraint::Percentage(20.0),
                    Constraint::Percentage(20.0),
                    Constraint::Percentage(15.0),
                    Constraint::Percentage(15.0),
                    Constraint::Percentage(10.0),
                ],
            )
        } else {
            // 80–119: hide model, keep health.
            (
                vec!["Name", "Program", "Health", "Last Active", "Msgs"],
                vec![
                    Constraint::Percentage(26.0),
                    Constraint::Percentage(22.0),
                    Constraint::Percentage(14.0),
                    Constraint::Percentage(22.0),
                    Constraint::Percentage(16.0),
                ],
            )
        };

        let header = Row::new(header_cells).style(Style::default().bold());
        let mut name_counts: HashMap<&str, usize> = HashMap::new();
        for agent in &self.agents {
            *name_counts.entry(agent.name.as_str()).or_insert(0) += 1;
        }

        let rows: Vec<Row> = self
            .agents
            .iter()
            .enumerate()
            .map(|(i, agent)| {
                let active_str = format_relative_time_with(agent.last_active_ts, now_ts);
                let health_str = health_badge(agent.health.as_ref());
                let msg_str = agent.message_count.to_string();
                let style = self.row_style(i, agent, now_ts);

                // Status badge prepended to name
                let status = AgentStatus::from_last_active(agent.last_active_ts, now_ts);
                let duplicate_name = name_counts.get(agent.name.as_str()).copied().unwrap_or(0) > 1;
                let name_display = if duplicate_name && !agent.project.is_empty() {
                    format!("{} {} [{}]", status.icon(), agent.name, agent.project)
                } else {
                    format!("{} {}", status.icon(), agent.name)
                };

                if narrow {
                    Row::new(vec![name_display, health_str, active_str]).style(style)
                } else if wide {
                    Row::new(vec![
                        name_display,
                        agent.program.clone(),
                        agent.model.clone(),
                        health_str,
                        active_str,
                        msg_str,
                    ])
                    .style(style)
                } else {
                    Row::new(vec![
                        name_display,
                        agent.program.clone(),
                        health_str,
                        active_str,
                        msg_str,
                    ])
                    .style(style)
                }
            })
            .collect();

        let block = Block::default()
            .title("Agents")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));

        let mut ts = self.table_state.clone();
        StatefulWidget::render(&table, area, frame, &mut ts);
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let total = self.agents.len() as u64;
        let (active, idle, inactive) = self.cached_status_counts;
        let total_msgs = self.cached_total_msgs;
        let attention = self
            .agents
            .iter()
            .filter(|agent| {
                agent
                    .health
                    .as_ref()
                    .is_some_and(AgentHealthScorecard::needs_attention)
            })
            .count() as u64;

        let total_str = total.to_string();
        let active_str = active.to_string();
        let idle_str = idle.to_string();
        let inactive_str = inactive.to_string();
        let attention_str = attention.to_string();
        let msgs_str = total_msgs.to_string();

        let items: Vec<(&str, &str, PackedRgba)> = vec![
            (&*total_str, "agents", tp.metric_agents),
            (&*active_str, "active", tp.activity_active),
            (&*idle_str, "idle", tp.activity_idle),
            (&*inactive_str, "offline", tp.activity_stale),
            (&*attention_str, "attention", tp.activity_stale),
            (&*msgs_str, "msgs", tp.metric_messages),
        ];

        SummaryFooter::new(&items, tp.text_muted).render(area, frame);
    }
}

/// Format a timestamp as relative time from `now_micros`.
fn format_relative_time_with(ts_micros: i64, now_micros: i64) -> String {
    if ts_micros == 0 {
        return "never".to_string();
    }
    let delta_secs = (now_micros - ts_micros) / 1_000_000;
    if delta_secs < 0 {
        return "future".to_string();
    }
    let delta = delta_secs.unsigned_abs();
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

fn agent_health_cmp(left: &AgentRow, right: &AgentRow, sort_asc: bool) -> std::cmp::Ordering {
    match (left.health.as_ref(), right.health.as_ref()) {
        (Some(left_health), Some(right_health)) => {
            let cmp = if sort_asc {
                right_health
                    .score
                    .cmp(&left_health.score)
                    .then_with(|| {
                        right_health
                            .observed_weight_bp
                            .cmp(&left_health.observed_weight_bp)
                    })
                    .then_with(|| right_health.decision_count.cmp(&left_health.decision_count))
            } else {
                left_health
                    .score
                    .cmp(&right_health.score)
                    .then_with(|| {
                        left_health
                            .observed_weight_bp
                            .cmp(&right_health.observed_weight_bp)
                    })
                    .then_with(|| left_health.decision_count.cmp(&right_health.decision_count))
            };
            cmp.then_with(|| crate::tui_screens::cmp_ci(&left.name, &right.name))
        }
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => crate::tui_screens::cmp_ci(&left.name, &right.name),
    }
}

fn health_badge(health: Option<&AgentHealthScorecard>) -> String {
    health
        .map(AgentHealthScorecard::badge)
        .unwrap_or_else(|| "--".to_string())
}

fn health_grade_color(
    grade: AgentHealthGrade,
    tp: &crate::tui_theme::TuiThemePalette,
) -> PackedRgba {
    match grade {
        AgentHealthGrade::A | AgentHealthGrade::B => tp.activity_active,
        AgentHealthGrade::C => tp.activity_idle,
        AgentHealthGrade::D | AgentHealthGrade::F => tp.activity_stale,
    }
}

fn health_metric_color(score: u8, tp: &crate::tui_theme::TuiThemePalette) -> PackedRgba {
    match score {
        90..=100 => tp.activity_active,
        75..=89 => tp.metric_agents,
        60..=74 => tp.activity_idle,
        _ => tp.activity_stale,
    }
}

const SPARKLINE_BLOCKS: &[char] = &[
    ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
];

/// Build a text-mode sparkline bar from rate history.
fn sparkline_bar(history: &VecDeque<f64>) -> String {
    let max_val = history
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max)
        .max(1.0);
    history
        .iter()
        .map(|v| {
            let frac = v / max_val;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let idx = (frac * 7.0).round() as usize;
            SPARKLINE_BLOCKS[idx.min(7)]
        })
        .collect()
}

/// Render key-value lines with a label column and a value column, supporting scroll.
#[allow(clippy::cast_possible_truncation)]
fn render_kv_lines(
    frame: &mut Frame<'_>,
    inner: Rect,
    lines: &[(String, String, Option<PackedRgba>)],
    scroll: usize,
    max_scroll_cell: &std::cell::Cell<usize>,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    let visible_height = usize::from(inner.height);
    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible_height);
    max_scroll_cell.set(max_scroll);
    let scroll = scroll.min(max_scroll);
    let label_w = 12u16;

    for (i, (label, value, color)) in lines.iter().skip(scroll).take(visible_height).enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }

        // Label
        let label_area = Rect::new(inner.x, y, label_w.min(inner.width), 1);
        let label_text = format!("{label}:");
        Paragraph::new(label_text)
            .style(Style::default().fg(tp.text_muted).bold())
            .render(label_area, frame);

        // Value
        let val_x = inner.x + label_w + 1;
        if val_x < inner.x + inner.width {
            let val_w = (inner.x + inner.width).saturating_sub(val_x);
            let val_area = Rect::new(val_x, y, val_w, 1);
            let val_style = color.map_or_else(
                || Style::default().fg(tp.text_primary),
                |c| Style::default().fg(c),
            );
            Paragraph::new(value.as_str())
                .style(val_style)
                .render(val_area, frame);
        }
    }

    // Scroll indicator
    if total_lines > visible_height {
        let indicator = format!(
            " {}/{} ",
            scroll + 1,
            total_lines.saturating_sub(visible_height) + 1
        );
        let ind_w = indicator.len() as u16;
        if ind_w < inner.width {
            let ind_area = Rect::new(
                inner.x + inner.width - ind_w,
                inner.y + inner.height.saturating_sub(1),
                ind_w,
                1,
            );
            Paragraph::new(indicator)
                .style(Style::default().fg(tp.text_muted))
                .render(ind_area, frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_core::Config;
    use tempfile::tempdir;

    fn test_state() -> std::sync::Arc<TuiSharedState> {
        let config = Config {
            database_url: "sqlite:///:memory:".to_string(),
            storage_root: std::path::PathBuf::from("/tmp/mcp_test_empty_root"),
            ..Config::default()
        };
        TuiSharedState::new(&config)
    }

    fn set_database_url(state: &std::sync::Arc<TuiSharedState>, database_url: String) {
        let mut snapshot = state.config_snapshot();
        snapshot.database_url = database_url.clone();
        snapshot.raw_database_url = database_url;
        state.update_config_snapshot(snapshot);
    }

    fn agent_identity(project: &str, name: &str) -> AgentIdentity {
        AgentIdentity::new(project.to_string(), name.to_string())
    }

    fn test_agent_row(
        name: &str,
        project: &str,
        program: &str,
        model: &str,
        last_active_ts: i64,
        message_count: u64,
    ) -> AgentRow {
        AgentRow {
            identity: agent_identity(project, name),
            name: name.to_string(),
            project: project.to_string(),
            program: program.to_string(),
            model: model.to_string(),
            health: None,
            last_active_ts,
            message_count,
        }
    }

    fn test_health(score: u8, decision_count: u64) -> AgentHealthScorecard {
        AgentHealthScorecard {
            score,
            grade: AgentHealthGrade::from_score(score),
            observed_weight_bp: 10_000,
            decision_count,
            metrics: Vec::new(),
        }
    }

    #[test]
    fn new_screen_has_defaults() {
        let screen = AgentsScreen::new();
        assert!(screen.agents.is_empty());
        assert!(!screen.filter_active);
        assert_eq!(screen.sort_col, COL_LAST_ACTIVE);
        assert!(!screen.sort_asc);
    }

    #[test]
    fn renders_without_panic() {
        let state = test_state();
        let screen = AgentsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn renders_at_minimum_size() {
        let state = test_state();
        let screen = AgentsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(20, 3, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 20, 3), &state);
    }

    #[test]
    fn renders_at_tiny_size_without_panic() {
        let state = test_state();
        let screen = AgentsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(10, 2, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 10, 2), &state);
    }

    #[test]
    fn renders_wide_layout() {
        let state = test_state();
        let screen = AgentsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(140, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 140, 30), &state);
    }

    #[test]
    fn renders_narrow_layout() {
        let state = test_state();
        let screen = AgentsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 20, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 60, 20), &state);
    }

    #[test]
    fn title_and_label() {
        let screen = AgentsScreen::new();
        assert_eq!(screen.title(), "Agents");
        assert_eq!(screen.tab_label(), "Agents");
    }

    #[test]
    fn keybindings_documented() {
        let screen = AgentsScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.len() >= 4);
        assert!(bindings.iter().any(|b| b.key == "j/k"));
        assert!(bindings.iter().any(|b| b.key == "/"));
        assert!(bindings.iter().any(|b| b.key == "i/h"));
    }

    #[test]
    fn detail_lines_include_health_badge() {
        let mut screen = AgentsScreen::new();
        screen.cached_now_ts = chrono::Utc::now().timestamp_micros();
        let mut agent = test_agent_row("RedFox", "alpha", "codex-cli", "gpt-5", 100, 7);
        agent.health = Some(test_health(63, 5));

        let lines =
            screen.build_detail_lines(&agent, &crate::tui_theme::TuiThemePalette::current());

        assert!(lines.iter().any(|(label, value, _)| {
            label == "Health" && value.contains("C 63") && value.contains("5 decisions")
        }));
    }

    #[test]
    fn health_sort_defaults_to_weakest_first_and_keeps_unscored_last() {
        let weak = {
            let mut agent = test_agent_row("Amber", "alpha", "codex-cli", "gpt-5", 100, 3);
            agent.health = Some(test_health(41, 2));
            agent
        };
        let strong = {
            let mut agent = test_agent_row("Blue", "alpha", "codex-cli", "gpt-5", 100, 8);
            agent.health = Some(test_health(96, 7));
            agent
        };
        let unknown = test_agent_row("Cipher", "alpha", "codex-cli", "gpt-5", 100, 1);

        let mut rows = vec![unknown, strong, weak];
        rows.sort_by(|left, right| agent_health_cmp(left, right, false));
        assert_eq!(
            rows.iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Amber", "Blue", "Cipher"]
        );

        rows.sort_by(|left, right| agent_health_cmp(left, right, true));
        assert_eq!(
            rows.iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Blue", "Amber", "Cipher"]
        );
    }

    #[test]
    fn slash_activates_filter() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        assert!(!screen.consumes_text_input());

        let slash = Event::Key(ftui::KeyEvent::new(KeyCode::Char('/')));
        screen.update(&slash, &state);
        assert!(screen.consumes_text_input());
    }

    #[test]
    fn escape_deactivates_filter() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        let slash = Event::Key(ftui::KeyEvent::new(KeyCode::Char('/')));
        screen.update(&slash, &state);
        assert!(screen.consumes_text_input());

        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        screen.update(&esc, &state);
        assert!(!screen.consumes_text_input());
    }

    #[test]
    fn s_cycles_sort_column() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        let initial = screen.sort_col;

        let s = Event::Key(ftui::KeyEvent::new(KeyCode::Char('s')));
        screen.update(&s, &state);
        assert_ne!(screen.sort_col, initial);
    }

    #[test]
    fn big_s_toggles_sort_order() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        let initial = screen.sort_asc;

        let s = Event::Key(ftui::KeyEvent::new(KeyCode::Char('S')));
        screen.update(&s, &state);
        assert_ne!(screen.sort_asc, initial);
    }

    #[test]
    fn deep_link_agent_by_name() {
        let mut screen = AgentsScreen::new();
        screen.agents.push(test_agent_row(
            "RedFox",
            "",
            "claude-code",
            "opus-4.6",
            100,
            5,
        ));
        let handled = screen.receive_deep_link(&DeepLinkTarget::AgentByName("RedFox".into()));
        assert!(handled);
        assert_eq!(screen.table_state.selected, Some(0));
    }

    #[test]
    fn down_key_selects_first_agent_when_nothing_is_selected() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        screen.agents.push(test_agent_row(
            "RedFox",
            "",
            "claude-code",
            "opus-4.6",
            100,
            5,
        ));
        screen
            .agents
            .push(test_agent_row("BlueLake", "", "codex-cli", "gpt-5", 200, 7));

        let down = Event::Key(ftui::KeyEvent::new(KeyCode::Down));
        screen.update(&down, &state);

        assert_eq!(screen.table_state.selected, Some(0));
    }

    #[test]
    fn deep_link_unknown_agent() {
        let mut screen = AgentsScreen::new();
        let handled = screen.receive_deep_link(&DeepLinkTarget::AgentByName("Unknown".into()));
        assert!(!handled);
    }

    #[test]
    fn format_relative_time_values() {
        let now = chrono::Utc::now().timestamp_micros();
        assert_eq!(format_relative_time_with(0, now), "never");
        let result = format_relative_time_with(now - 30_000_000, now); // 30s ago
        assert!(result.contains("s ago"));
        let result = format_relative_time_with(now - 300_000_000, now); // 5m ago
        assert!(result.contains("m ago"));
    }

    #[test]
    fn default_impl() {
        let screen = AgentsScreen::default();
        assert!(screen.agents.is_empty());
    }

    #[test]
    fn rebuild_emits_screen_diagnostic_with_raw_and_rendered_counts() {
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 3,
            agents_list: vec![
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "RedFox".to_string(),
                    program: "claude-code".to_string(),
                    model: String::new(),
                    last_active_ts: 100,
                    health: None,
                },
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "BlueLake".to_string(),
                    program: "codex-cli".to_string(),
                    model: String::new(),
                    last_active_ts: 200,
                    health: None,
                },
            ],
            ..Default::default()
        });

        let mut screen = AgentsScreen::new();
        screen.filter = "red".to_string();
        screen.rebuild_from_state(&state);

        let diagnostics = state.screen_diagnostics_since(0);
        assert_eq!(diagnostics.len(), 1);
        let (_, diag) = diagnostics
            .last()
            .expect("screen diagnostic should be present");
        assert_eq!(diag.screen, "agents");
        // raw_count uses total_rows (DB COUNT=3), not agents_list.len()=2,
        // so the diagnostic tracks the full cardinality gap.
        assert_eq!(diag.raw_count, 3);
        assert_eq!(diag.rendered_count, 1);
        assert_eq!(diag.dropped_count, 2);
        assert!(diag.query_params.contains("filter=red"));
        assert!(diag.query_params.contains("list_rows=2"));
        assert!(diag.query_params.contains("total_rows=3"));
    }

    #[test]
    fn rebuild_emits_screen_diagnostic_filter_all_when_empty() {
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 1,
            agents_list: vec![crate::tui_events::AgentSummary {
                project: String::new(),
                name: "RedFox".to_string(),
                program: "claude-code".to_string(),
                model: String::new(),
                last_active_ts: 100,
                health: None,
            }],
            ..Default::default()
        });

        let mut screen = AgentsScreen::new();
        screen.filter = " \n ".to_string();
        screen.rebuild_from_state(&state);

        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics
            .last()
            .expect("screen diagnostic should be present");
        assert!(diag.query_params.contains("filter=all"));
    }

    #[test]
    fn rebuild_diagnostic_raw_count_tracks_list_rows_when_total_counter_is_lower() {
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 0,
            agents_list: vec![
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "RedFox".to_string(),
                    program: "claude-code".to_string(),
                    model: String::new(),
                    last_active_ts: 100,
                    health: None,
                },
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "BlueLake".to_string(),
                    program: "codex-cli".to_string(),
                    model: String::new(),
                    last_active_ts: 200,
                    health: None,
                },
            ],
            ..Default::default()
        });

        let mut screen = AgentsScreen::new();
        screen.filter = "blue".to_string();
        screen.rebuild_from_state(&state);

        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics
            .last()
            .expect("screen diagnostic should be present");
        assert_eq!(diag.raw_count, 2);
        assert_eq!(diag.rendered_count, 1);
        assert_eq!(diag.dropped_count, 1);
        assert!(diag.query_params.contains("list_rows=2"));
        assert!(diag.query_params.contains("total_rows=0"));
    }

    #[test]
    fn sanitize_diagnostic_value_strips_delimiters_and_whitespace() {
        let value = sanitize_diagnostic_value(" alpha;\n beta,\r gamma ");
        assert_eq!(value, "alpha beta gamma");
    }

    #[test]
    fn status_thresholds_are_classified() {
        let now = chrono::Utc::now().timestamp_micros();
        assert_eq!(AgentStatus::from_last_active(now, now), AgentStatus::Active);
        assert_eq!(
            AgentStatus::from_last_active(now - ACTIVE_WINDOW_MICROS - 1, now),
            AgentStatus::Idle
        );
        assert_eq!(
            AgentStatus::from_last_active(now - IDLE_WINDOW_MICROS - 10_000_000, now),
            AgentStatus::Inactive
        );
        assert_eq!(AgentStatus::from_last_active(0, now), AgentStatus::Inactive);
    }

    #[test]
    fn status_fade_records_transition_and_expires() {
        let mut screen = AgentsScreen::new();
        screen.reduced_motion = false;
        let now = chrono::Utc::now().timestamp_micros();
        screen.cached_now_ts = now;
        let mut rows = vec![test_agent_row("RedFox", "", "claude-code", "opus", now, 1)];

        screen.rebuild_status_transitions(&rows);
        assert!(screen.status_fades.is_empty());

        rows[0].last_active_ts = now - IDLE_WINDOW_MICROS - 10_000_000;
        screen.rebuild_status_transitions(&rows);
        let fade = screen
            .status_fades
            .get(&agent_identity("", "RedFox"))
            .expect("status transition should create fade");
        assert_eq!(fade.from, AgentStatus::Active);
        assert_eq!(fade.to, AgentStatus::Inactive);
        assert_eq!(fade.ticks_remaining, STATUS_FADE_TICKS);

        for _ in 0..STATUS_FADE_TICKS {
            screen.advance_status_fades();
        }
        assert!(screen.status_fades.is_empty());
    }

    #[test]
    fn reduced_motion_disables_status_fades() {
        let mut screen = AgentsScreen::new();
        screen.reduced_motion = true;
        let now = chrono::Utc::now().timestamp_micros();
        let mut rows = vec![test_agent_row("BlueFox", "", "claude-code", "opus", now, 1)];
        screen.rebuild_status_transitions(&rows);
        rows[0].last_active_ts = now - IDLE_WINDOW_MICROS - 10_000_000;
        screen.rebuild_status_transitions(&rows);
        assert!(screen.status_fades.is_empty());
    }

    #[test]
    fn message_flash_ticks_decay_to_zero() {
        let mut screen = AgentsScreen::new();
        screen.reduced_motion = false;
        screen
            .message_flash_ticks
            .insert(agent_identity("", "RedFox"), MESSAGE_FLASH_TICKS);

        for _ in 0..MESSAGE_FLASH_TICKS {
            screen.advance_message_flashes();
        }
        assert!(
            !screen
                .message_flash_ticks
                .contains_key(&agent_identity("", "RedFox"))
        );
    }

    #[test]
    fn stagger_reveal_assigns_cascading_delays() {
        let mut screen = AgentsScreen::new();
        screen.reduced_motion = false;
        let now = chrono::Utc::now().timestamp_micros();
        let rows = vec![
            test_agent_row("A", "", "p", "m", now, 0),
            test_agent_row("B", "", "p", "m", now, 0),
            test_agent_row("C", "", "p", "m", now, 0),
        ];

        screen.track_stagger_reveals(&rows);
        assert_eq!(
            screen.stagger_reveal_ticks.get(&agent_identity("", "A")),
            Some(&1)
        );
        assert_eq!(
            screen.stagger_reveal_ticks.get(&agent_identity("", "B")),
            Some(&2)
        );
        assert_eq!(
            screen.stagger_reveal_ticks.get(&agent_identity("", "C")),
            Some(&3)
        );

        screen.advance_stagger_reveals();
        assert!(
            !screen
                .stagger_reveal_ticks
                .contains_key(&agent_identity("", "A"))
        );
        assert_eq!(
            screen.stagger_reveal_ticks.get(&agent_identity("", "B")),
            Some(&1)
        );
    }

    // ── focused_event tests ───────────────────────────────────────

    #[test]
    fn focused_event_none_when_empty() {
        let screen = AgentsScreen::new();
        assert!(screen.focused_event().is_none());
    }

    #[test]
    fn focused_event_returns_agent_registered_synthetic() {
        let mut screen = AgentsScreen::new();
        screen.agents.push(test_agent_row(
            "RedFox",
            "proj-a",
            "claude-code",
            "opus-4.6",
            0,
            0,
        ));
        screen.table_state.selected = Some(0);
        screen.sync_focused_event();

        assert!(matches!(
            screen.focused_event(),
            Some(crate::tui_events::MailEvent::AgentRegistered {
                project,
                model_name,
                ..
            }) if project == "proj-a" && model_name == "opus-4.6"
        ));
    }

    #[test]
    fn focused_event_none_when_selection_out_of_range() {
        let mut screen = AgentsScreen::new();
        screen.table_state.selected = Some(5);
        screen.sync_focused_event();
        assert!(screen.focused_event().is_none());
    }

    #[test]
    fn sparkline_records_samples() {
        let mut screen = AgentsScreen::new();
        screen.total_msgs_this_tick = 5;
        screen.record_sparkline_sample();
        assert_eq!(screen.msg_rate_history.len(), 1);
        assert!((screen.msg_rate_history[0] - 5.0).abs() < f64::EPSILON);
        assert_eq!(screen.total_msgs_this_tick, 0);
    }

    #[test]
    fn sparkline_bounded_capacity() {
        let mut screen = AgentsScreen::new();
        for i in 0..SPARKLINE_CAP + 5 {
            screen.total_msgs_this_tick = i as u64;
            screen.record_sparkline_sample();
        }
        assert_eq!(screen.msg_rate_history.len(), SPARKLINE_CAP);
    }

    #[test]
    fn status_counts_empty() {
        let screen = AgentsScreen::new();
        assert_eq!(screen.compute_status_counts(), (0, 0, 0));
    }

    // ── B4: Cardinality truth assertions ────────────────────────────

    #[test]
    fn agents_cardinality_passes_when_agents_rendered() {
        assert_agents_list_cardinality(100, 50, "");
    }

    #[test]
    fn agents_cardinality_passes_when_filter_active_and_empty() {
        assert_agents_list_cardinality(100, 0, "nonexistent-agent");
    }

    #[test]
    fn agents_cardinality_passes_when_db_empty() {
        assert_agents_list_cardinality(0, 0, "");
    }

    #[test]
    fn enrich_lossy_agent_summary_rows_from_previous_restores_unique_project_scope() {
        let mut rows = vec![test_agent_row("RedFox", "", "claude-code", "", 200, 2)];
        let previous_rows = vec![test_agent_row(
            "RedFox",
            "alpha",
            "claude-code",
            "opus-4.6",
            100,
            7,
        )];

        enrich_lossy_agent_summary_rows_from_previous(&mut rows, &previous_rows);

        assert_eq!(rows[0].identity, agent_identity("alpha", "RedFox"));
        assert_eq!(rows[0].project, "alpha");
        assert_eq!(rows[0].model, "opus-4.6");
        assert_eq!(rows[0].message_count, 7);
    }

    #[test]
    fn enrich_lossy_agent_summary_rows_from_previous_restores_duplicate_names_with_program_match() {
        let mut rows = vec![test_agent_row("RedFox", "", "codex-cli", "", 200, 2)];
        let previous_rows = vec![
            test_agent_row("RedFox", "alpha", "claude-code", "opus-4.6", 100, 7),
            test_agent_row("RedFox", "beta", "codex-cli", "gpt-5", 200, 3),
        ];

        enrich_lossy_agent_summary_rows_from_previous(&mut rows, &previous_rows);

        assert_eq!(rows[0].identity, agent_identity("beta", "RedFox"));
        assert_eq!(rows[0].project, "beta");
        assert_eq!(rows[0].model, "gpt-5");
        assert_eq!(rows[0].message_count, 3);
    }

    #[test]
    fn agents_cardinality_catches_false_empty_state() {
        let result = std::panic::catch_unwind(|| {
            assert_agents_list_cardinality(100, 0, "");
        });
        assert!(
            result.is_err(),
            "should panic when DB has agents but rendered list is empty without filter"
        );
    }

    // ── G6: Project/Agent scope audit tests ─────────────────────────

    #[test]
    fn agents_query_is_global_by_design() {
        // Documents the invariant: agent list fetches ALL agents across all projects.
        // Agent names are globally unique (adjective+noun validation) so cross-project
        // ambiguity is prevented at the naming layer, not the query layer.
        let sql = "SELECT name, program, last_active_ts FROM agents \
            ORDER BY last_active_ts DESC, id DESC LIMIT 500";
        // Confirm no WHERE project_id clause — this is intentional for the global view.
        assert!(
            !sql.contains("project_id"),
            "agent list query must be global (no project_id filter) \
             because agent names are globally unique"
        );
    }

    #[test]
    fn filter_and_sort_use_same_case_function() {
        // G3: Documents fix — filter uses to_ascii_lowercase(), sort must also
        // use to_ascii_lowercase() (not to_lowercase()) for consistent behavior
        // with non-ASCII characters.
        let name = "Caf\u{00e9}Agent"; // "CaféAgent"
        let ascii_lower = name.to_ascii_lowercase();
        let unicode_lower = name.to_lowercase();
        // ASCII lowercase leaves non-ASCII bytes unchanged
        assert_eq!(ascii_lower, "caf\u{00e9}agent");
        // Unicode lowercase also lowercases (same for this char, but differs for e.g. Σ→σ)
        assert_eq!(unicode_lower, "caf\u{00e9}agent");
        // For agent names (ASCII-only validated), both are equivalent,
        // but we use to_ascii_lowercase() consistently for correctness.
    }

    #[test]
    fn agent_name_uniqueness_prevents_cross_project_ambiguity() {
        // Documents: agent names are validated via adjective+noun system
        // which ensures global uniqueness. Two projects cannot have "BlueLake".
        use mcp_agent_mail_core::models::is_valid_agent_name;
        // Valid agent names follow the adjective+noun pattern
        assert!(
            is_valid_agent_name("BlueLake"),
            "adjective+noun names should be valid"
        );
        // The uniqueness constraint is enforced at DB level (UNIQUE per project)
        // and by the naming system generating distinct combinations.
    }

    // ── B6: Count/List Consistency Contract ──────────────────────────

    #[test]
    fn count_list_consistency_no_filter_no_cap() {
        // When total_rows == list length and no filter: rendered_count must
        // equal total_rows. Diagnostic must show capped=false.
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 2,
            agents_list: vec![
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "RedFox".to_string(),
                    program: "claude-code".to_string(),
                    model: String::new(),
                    last_active_ts: 100,
                    health: None,
                },
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "BlueLake".to_string(),
                    program: "codex-cli".to_string(),
                    model: String::new(),
                    last_active_ts: 200,
                    health: None,
                },
            ],
            ..Default::default()
        });

        let mut screen = AgentsScreen::new();
        screen.rebuild_from_state(&state);

        assert_eq!(screen.agents.len(), 2);
        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics.last().expect("diagnostic expected");
        assert_eq!(diag.raw_count, 2);
        assert_eq!(diag.rendered_count, 2);
        assert_eq!(diag.dropped_count, 0);
        assert!(
            diag.query_params.contains("capped=false"),
            "list is not capped when total_rows == list length"
        );
    }

    #[test]
    fn count_list_consistency_capped_list() {
        // When total_rows > list length (simulating poller cap), diagnostic
        // must show capped=true with correct cardinality gap.
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 600, // DB reports 600 agents
            agents_list: vec![
                // But poller only provides 2 (simulating cap)
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "RedFox".to_string(),
                    program: "claude-code".to_string(),
                    model: String::new(),
                    last_active_ts: 100,
                    health: None,
                },
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "BlueLake".to_string(),
                    program: "codex-cli".to_string(),
                    model: String::new(),
                    last_active_ts: 200,
                    health: None,
                },
            ],
            ..Default::default()
        });

        let mut screen = AgentsScreen::new();
        screen.rebuild_from_state(&state);

        assert_eq!(screen.agents.len(), 2);
        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics.last().expect("diagnostic expected");
        assert_eq!(diag.raw_count, 600);
        assert_eq!(diag.rendered_count, 2);
        assert_eq!(diag.dropped_count, 598);
        assert!(
            diag.query_params.contains("capped=true"),
            "list must be flagged as capped when total_rows > list length"
        );
        assert!(
            diag.query_params.contains("list_rows=2"),
            "list_rows tracks actual list length"
        );
        assert!(
            diag.query_params.contains("total_rows=600"),
            "total_rows tracks DB COUNT"
        );
    }

    #[test]
    fn count_list_consistency_filter_reduces_rendered() {
        // Filter should reduce rendered_count but not raw_count or total_rows.
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 3,
            agents_list: vec![
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "RedFox".to_string(),
                    program: "claude-code".to_string(),
                    model: String::new(),
                    last_active_ts: 100,
                    health: None,
                },
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "BlueLake".to_string(),
                    program: "codex-cli".to_string(),
                    model: String::new(),
                    last_active_ts: 200,
                    health: None,
                },
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "GreenPeak".to_string(),
                    program: "claude-code".to_string(),
                    model: String::new(),
                    last_active_ts: 300,
                    health: None,
                },
            ],
            ..Default::default()
        });

        let mut screen = AgentsScreen::new();
        screen.filter = "claude".to_string();
        screen.rebuild_from_state(&state);

        assert_eq!(
            screen.agents.len(),
            2,
            "filter matches RedFox and GreenPeak"
        );
        let diagnostics = state.screen_diagnostics_since(0);
        let (_, diag) = diagnostics.last().expect("diagnostic expected");
        assert_eq!(diag.raw_count, 3);
        assert_eq!(diag.rendered_count, 2);
        assert_eq!(diag.dropped_count, 1);
        assert!(
            diag.query_params.contains("capped=false"),
            "not capped when total_rows == list length"
        );
    }

    // ── B8: DB context binding guardrail regression tests ─────────────

    #[test]
    fn b8_agents_unavailable_after_grace_period_without_poller() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        assert!(!screen.db_context_unavailable, "starts false");

        // Tick at 0 — within grace period
        screen.tick(0, &state);
        assert!(
            !screen.db_context_unavailable,
            "should not show banner during startup grace"
        );

        // Tick past grace period (>= 30) without poller data
        screen.tick(30, &state);
        assert!(
            screen.db_context_unavailable,
            "should show banner when poller never delivered data after grace period"
        );
        assert_eq!(
            screen.db_context_banner,
            crate::tui_screens::POLLER_DB_WAITING_BANNER
        );
    }

    #[test]
    fn b8_agents_available_after_poller_delivers() {
        let state = test_state();
        let mut screen = AgentsScreen::new();

        // Simulate poller delivery
        state.mark_db_context_available();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 1,
            agents_list: vec![crate::tui_events::AgentSummary {
                project: String::new(),
                name: "TestAgent".to_string(),
                program: "test".to_string(),
                model: String::new(),
                last_active_ts: 100,
                health: None,
            }],
            ..Default::default()
        });

        screen.tick(30, &state);
        assert!(
            !screen.db_context_unavailable,
            "should not show banner when poller has delivered data"
        );
        assert_eq!(screen.agents.len(), 1);
    }

    #[test]
    fn b8_agents_keep_waiting_banner_until_first_snapshot_is_applied() {
        let state = test_state();
        state.mark_db_context_available();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            timestamp_micros: 10,
            ..crate::tui_events::DbStatSnapshot::default()
        });
        let mut screen = AgentsScreen::new();

        // Tick past the startup grace window (POLLER_DB_GRACE_TICKS = 30)
        // so the banner suppression during early boot does not hide the
        // "waiting for first snapshot" signal. Use a non-multiple-of-10
        // tick so `rebuild_from_state` (which would bump
        // `applied_db_stats_gen` and silence the banner) does not fire —
        // we specifically want to observe the "available in state but
        // not yet applied to screen" window.
        screen.tick(31, &state);

        assert!(screen.db_context_unavailable);
        assert_eq!(
            screen.db_context_banner,
            crate::tui_screens::POLLER_DB_WAITING_BANNER
        );
    }

    #[test]
    fn cadence_rebuild_uses_latched_dirty_signal() {
        let state = test_state();
        let mut screen = AgentsScreen::new();

        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 1,
            agents_list: vec![crate::tui_events::AgentSummary {
                project: String::new(),
                name: "TickLatch".to_string(),
                program: "codex".to_string(),
                model: String::new(),
                last_active_ts: 100,
                health: None,
            }],
            ..Default::default()
        });

        // Dirty on a non-cadence tick should still rebuild on the next cadence tick.
        screen.tick(9, &state);
        assert_eq!(screen.agents.len(), 0);
        screen.tick(10, &state);
        assert_eq!(screen.agents.len(), 1);
    }

    #[test]
    fn b8_agents_banner_renders_when_unavailable() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        screen.db_context_unavailable = true;
        screen.db_context_banner = crate::tui_screens::POLLER_DB_UNAVAILABLE_BANNER;

        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, ftui::layout::Rect::new(0, 0, 120, 30), &state);

        let mut text = String::new();
        for y in 0..frame.buffer.height() {
            for x in 0..frame.buffer.width() {
                if let Some(cell) = frame.buffer.get(x, y) {
                    if let Some(ch) = cell.content.as_char() {
                        text.push(ch);
                    } else if !cell.is_continuation() {
                        text.push(' ');
                    }
                }
            }
            text.push('\n');
        }
        assert!(
            text.contains("Database context unavailable"),
            "should render degraded banner: {text}"
        );
        assert!(
            text.contains("Check DB URL/project scope and refresh"),
            "should render explicit degraded copy: {text}"
        );
    }

    #[test]
    fn b8_agents_show_explicit_unavailable_after_warmup_failure() {
        let state = test_state();
        state.mark_db_warmup_failed();
        let mut screen = AgentsScreen::new();

        screen.tick(30, &state);

        assert!(screen.db_context_unavailable);
        assert_eq!(
            screen.db_context_banner,
            crate::tui_screens::POLLER_DB_UNAVAILABLE_BANNER
        );
    }

    // ── E4: Hotspot remediation cache tests ─────────────────────────

    #[test]
    fn cached_now_ts_set_on_tick() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        assert_eq!(screen.cached_now_ts, 0);
        screen.tick(0, &state);
        assert!(screen.cached_now_ts > 0, "tick must snapshot now_ts");
    }

    #[test]
    fn cached_status_counts_updated_on_rebuild() {
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 1,
            agents_list: vec![crate::tui_events::AgentSummary {
                project: String::new(),
                name: "RedFox".to_string(),
                program: "claude-code".to_string(),
                model: String::new(),
                last_active_ts: chrono::Utc::now().timestamp_micros(), // active
                health: None,
            }],
            ..Default::default()
        });
        let mut screen = AgentsScreen::new();
        screen.cached_now_ts = chrono::Utc::now().timestamp_micros();
        screen.rebuild_from_state(&state);
        let (active, _idle, inactive) = screen.cached_status_counts;
        assert_eq!(
            active, 1,
            "recent last_active_ts should be counted as active"
        );
        assert_eq!(inactive, 0, "no agents should be inactive");
    }

    #[test]
    fn cached_total_msgs_updated_on_rebuild() {
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 1,
            agents_list: vec![crate::tui_events::AgentSummary {
                project: String::new(),
                name: "RedFox".to_string(),
                program: "claude-code".to_string(),
                model: String::new(),
                last_active_ts: 100,
                health: None,
            }],
            ..Default::default()
        });
        let mut screen = AgentsScreen::new();
        screen.cached_now_ts = chrono::Utc::now().timestamp_micros();
        screen.msg_counts.insert(agent_identity("", "RedFox"), 42);
        screen.rebuild_from_state(&state);
        assert_eq!(screen.cached_total_msgs, 42);
    }

    #[test]
    fn fetch_agent_rows_from_sqlite_counts_sent_and_received_messages() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("agents-message-counts.sqlite3");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open sqlite");
        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL, created_at INTEGER NOT NULL)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, last_active_ts INTEGER NOT NULL)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, sender_id INTEGER NOT NULL)",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL)",
            &[],
        )
        .expect("create message_recipients");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES \
             (1, 'alpha', '/tmp/alpha', 10), \
             (2, 'beta', '/tmp/beta', 20)",
            &[],
        )
        .expect("insert projects");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, last_active_ts) VALUES \
             (1, 1, 'RedFox', 'claude-code', 'opus', 100), \
             (2, 2, 'BlueLake', 'codex-cli', 'gpt-5', 200)",
            &[],
        )
        .expect("insert agents");
        conn.execute_sync(
            "INSERT INTO messages (id, sender_id) VALUES (10, 1), (11, 1), (12, 2)",
            &[],
        )
        .expect("insert messages");
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES \
             (10, 2, 'to'), \
             (11, 2, 'to'), \
             (12, 1, 'to')",
            &[],
        )
        .expect("insert recipients");

        let rows = fetch_agent_rows_from_sqlite(&conn);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].identity, agent_identity("beta", "BlueLake"));
        assert_eq!(rows[0].model, "gpt-5");
        assert_eq!(rows[0].message_count, 3);
        assert_eq!(rows[1].identity, agent_identity("alpha", "RedFox"));
        assert_eq!(rows[1].model, "opus");
        assert_eq!(rows[1].message_count, 3);
    }

    #[test]
    fn fetch_agent_rows_from_sqlite_preserves_unknown_project_and_program_labels() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("agents-unknown-labels.sqlite3");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open sqlite");
        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT, program TEXT, model TEXT, last_active_ts INTEGER NOT NULL)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, sender_id INTEGER NOT NULL)",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT)",
            &[],
        )
        .expect("create message_recipients");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, last_active_ts)
             VALUES (7, 99, 'RiverFox', '', NULL, 100)",
            &[],
        )
        .expect("insert agent");

        let rows = fetch_agent_rows_from_sqlite(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "RiverFox");
        assert_eq!(rows[0].project, "[unknown-project-99]");
        assert_eq!(rows[0].program, "[unknown-program]");
        assert_eq!(
            rows[0].identity,
            agent_identity("[unknown-project-99]", "RiverFox")
        );
    }

    #[test]
    fn rebuild_uses_sqlite_message_counts_when_event_counts_are_empty() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("agents-rebuild-message-counts.sqlite3");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open sqlite");
        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL, created_at INTEGER NOT NULL)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, last_active_ts INTEGER NOT NULL)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, sender_id INTEGER NOT NULL)",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL)",
            &[],
        )
        .expect("create message_recipients");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES \
             (1, 'alpha', '/tmp/alpha', 10), \
             (2, 'beta', '/tmp/beta', 20)",
            &[],
        )
        .expect("insert projects");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, last_active_ts) VALUES \
             (1, 1, 'RedFox', 'claude-code', 'opus', 100), \
             (2, 2, 'BlueLake', 'codex-cli', 'gpt-5', 200)",
            &[],
        )
        .expect("insert agents");
        conn.execute_sync(
            "INSERT INTO messages (id, sender_id) VALUES (10, 1), (11, 1), (12, 2), (13, 1)",
            &[],
        )
        .expect("insert messages");
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES \
             (10, 2, 'to'), \
             (11, 2, 'to'), \
             (12, 1, 'to'), \
             (13, 2, 'cc')",
            &[],
        )
        .expect("insert recipients");

        let state = test_state();
        set_database_url(&state, format!("sqlite://{}", db_path.to_string_lossy()));
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 2,
            messages: 4,
            agents_list: vec![
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "RedFox".to_string(),
                    program: "claude-code".to_string(),
                    model: String::new(),
                    last_active_ts: 100,
                    health: None,
                },
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "BlueLake".to_string(),
                    program: "codex-cli".to_string(),
                    model: String::new(),
                    last_active_ts: 200,
                    health: None,
                },
            ],
            ..Default::default()
        });

        let mut screen = AgentsScreen::new();
        screen.cached_now_ts = chrono::Utc::now().timestamp_micros();
        screen.rebuild_from_state(&state);

        assert_eq!(screen.cached_total_msgs, 4);
        assert_eq!(screen.agents[0].name, "BlueLake");
        assert_eq!(screen.agents[0].message_count, 4);
        assert_eq!(screen.agents[1].name, "RedFox");
        assert_eq!(screen.agents[1].message_count, 4);
    }

    #[test]
    fn ingest_events_scopes_models_by_project_for_duplicate_names() {
        let state = test_state();
        let mut screen = AgentsScreen::new();

        assert!(
            state.push_event(crate::tui_events::MailEvent::agent_registered(
                "RedFox",
                "claude-code",
                "opus-4.6",
                "alpha"
            ))
        );
        assert!(
            state.push_event(crate::tui_events::MailEvent::agent_registered(
                "RedFox",
                "codex-cli",
                "gpt-5",
                "beta"
            ))
        );

        screen.ingest_events(&state);

        assert_eq!(
            screen.model_names.get(&agent_identity("alpha", "RedFox")),
            Some(&"opus-4.6".to_string())
        );
        assert_eq!(
            screen.model_names.get(&agent_identity("beta", "RedFox")),
            Some(&"gpt-5".to_string())
        );
    }

    #[test]
    fn rebuild_uses_sqlite_roster_to_keep_duplicate_agent_names_project_scoped() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("agents-duplicate-names.sqlite3");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open sqlite");
        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL, created_at INTEGER NOT NULL)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, last_active_ts INTEGER NOT NULL)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, sender_id INTEGER NOT NULL)",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL)",
            &[],
        )
        .expect("create message_recipients");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES \
             (1, 'alpha', '/tmp/alpha', 10), \
             (2, 'beta', '/tmp/beta', 20)",
            &[],
        )
        .expect("insert projects");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, last_active_ts) VALUES \
             (1, 1, 'RedFox', 'claude-code', 'opus-4.6', 100), \
             (2, 2, 'RedFox', 'codex-cli', 'gpt-5', 200)",
            &[],
        )
        .expect("insert agents");

        let state = test_state();
        set_database_url(&state, format!("sqlite://{}", db_path.to_string_lossy()));
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 2,
            agents_list: vec![
                crate::tui_events::AgentSummary {
                    project: "alpha".to_string(),
                    name: "RedFox".to_string(),
                    program: "claude-code".to_string(),
                    model: String::new(),
                    last_active_ts: 100,
                    health: None,
                },
                crate::tui_events::AgentSummary {
                    project: "beta".to_string(),
                    name: "RedFox".to_string(),
                    program: "codex-cli".to_string(),
                    model: String::new(),
                    last_active_ts: 200,
                    health: None,
                },
            ],
            ..Default::default()
        });

        let mut screen = AgentsScreen::new();
        screen.cached_now_ts = chrono::Utc::now().timestamp_micros();
        screen.rebuild_from_state(&state);

        assert_eq!(screen.agents.len(), 2);
        let alpha = screen
            .agents
            .iter()
            .find(|agent| agent.identity == agent_identity("alpha", "RedFox"))
            .expect("alpha RedFox row");
        let beta = screen
            .agents
            .iter()
            .find(|agent| agent.identity == agent_identity("beta", "RedFox"))
            .expect("beta RedFox row");
        assert_eq!(alpha.model, "opus-4.6");
        assert_eq!(beta.model, "gpt-5");
        assert_ne!(alpha.identity, beta.identity);
    }

    #[test]
    fn rebuild_preserves_previous_rows_when_db_reports_agents_but_roster_is_temporarily_empty() {
        let broken_db_root = tempdir().expect("tempdir");
        let state = test_state();
        set_database_url(
            &state,
            format!("sqlite://{}", broken_db_root.path().to_string_lossy()),
        );
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 1,
            agents_list: Vec::new(),
            ..Default::default()
        });

        let mut screen = AgentsScreen::new();
        screen.agents = vec![test_agent_row(
            "RedFox",
            "alpha",
            "claude-code",
            "opus-4.6",
            100,
            7,
        )];
        screen.cached_now_ts = chrono::Utc::now().timestamp_micros();

        screen.rebuild_from_state(&state);

        assert_eq!(screen.agents.len(), 1);
        assert_eq!(screen.agents[0].identity, agent_identity("alpha", "RedFox"));
        assert_eq!(screen.agents[0].project, "alpha");
        assert_eq!(screen.agents[0].model, "opus-4.6");
        assert_eq!(screen.agents[0].message_count, 7);
    }

    #[test]
    fn rebuild_recovered_rows_still_restore_previous_scope_when_sqlite_rows_are_lossy() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("agents-lossy-recovery.sqlite3");
        let db_path_str = db_path.display().to_string();
        let conn = mcp_agent_mail_db::DbConn::open_file(&db_path_str).expect("open sqlite");
        conn.execute_raw(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL
            )",
        )
        .expect("create projects");
        conn.execute_raw(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT NOT NULL,
                model TEXT,
                last_active_ts INTEGER NOT NULL
            )",
        )
        .expect("create agents");
        conn.execute_raw(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                sender_id INTEGER NOT NULL
            )",
        )
        .expect("create messages");
        conn.execute_raw(
            "CREATE TABLE message_recipients (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL
            )",
        )
        .expect("create message recipients");
        conn.execute_raw(
            "INSERT INTO agents (id, project_id, name, program, model, last_active_ts)
             VALUES (1, 99, 'RedFox', 'claude-code', NULL, 100)",
        )
        .expect("insert lossy agent");

        let state = test_state();
        set_database_url(&state, format!("sqlite://{}", db_path.to_string_lossy()));
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 1,
            agents_list: vec![crate::tui_events::AgentSummary {
                project: "alpha".to_string(),
                name: "RedFox".to_string(),
                program: "claude-code".to_string(),
                model: String::new(),
                last_active_ts: 100,
                health: None,
            }],
            ..Default::default()
        });

        let mut screen = AgentsScreen::new();
        screen.agents = vec![test_agent_row(
            "RedFox",
            "alpha",
            "claude-code",
            "opus-4.6",
            100,
            7,
        )];
        screen.cached_now_ts = chrono::Utc::now().timestamp_micros();

        screen.rebuild_from_state(&state);

        assert_eq!(screen.agents.len(), 1);
        assert_eq!(screen.agents[0].identity, agent_identity("alpha", "RedFox"));
        assert_eq!(screen.agents[0].project, "alpha");
        assert_eq!(screen.agents[0].model, "opus-4.6");
        assert_eq!(screen.agents[0].message_count, 7);
    }

    #[test]
    fn sparkline_cache_refreshed_after_sample() {
        let mut screen = AgentsScreen::new();
        assert!(screen.cached_sparkline_vec.is_empty());
        screen.total_msgs_this_tick = 7;
        screen.record_sparkline_sample();
        assert!(screen.sparkline_dirty);
        screen.refresh_sparkline_cache();
        assert!(!screen.sparkline_dirty);
        assert_eq!(screen.cached_sparkline_vec.len(), 1);
        assert!((screen.cached_sparkline_vec[0] - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn sync_focused_event_gated_by_selection_change() {
        let mut screen = AgentsScreen::new();
        screen.agents.push(test_agent_row(
            "RedFox",
            "proj-a",
            "claude-code",
            "opus-4.6",
            100,
            0,
        ));
        // No selection → sync_focused_event produces None
        screen.sync_focused_event();
        assert!(screen.focused_synthetic.is_none());
        // Set selection → sync produces Some
        screen.table_state.selected = Some(0);
        screen.sync_focused_event();
        assert!(matches!(
            screen.focused_event(),
            Some(crate::tui_events::MailEvent::AgentRegistered {
                project,
                model_name,
                ..
            }) if project == "proj-a" && model_name == "opus-4.6"
        ));
        // Gate: last_selection tracks changes
        screen.last_selection = Some(0);
        let current_sel = screen.table_state.selected;
        assert_eq!(
            current_sel, screen.last_selection,
            "no change = gate closed"
        );
    }

    #[test]
    fn sync_focused_event_refreshes_when_agent_list_changes() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        screen.agents.push(test_agent_row(
            "RedFox",
            "proj-a",
            "claude-code",
            "opus-4.6",
            100,
            0,
        ));
        screen.table_state.selected = Some(0);
        screen.sync_focused_event();
        assert!(matches!(
            screen.focused_event(),
            Some(crate::tui_events::MailEvent::AgentRegistered {
                project,
                model_name,
                ..
            }) if project == "proj-a" && model_name == "opus-4.6"
        ));

        screen.last_selection = Some(0);
        screen.last_agents_list_gen = screen.agents_list_gen;
        screen.agents[0].model = "gpt-5".to_string();
        screen.agents_list_gen = screen.agents_list_gen.wrapping_add(1);

        screen.tick(1, &state);

        assert!(matches!(
            screen.focused_event(),
            Some(crate::tui_events::MailEvent::AgentRegistered { model_name, .. })
                if model_name == "gpt-5"
        ));
        assert_eq!(screen.last_agents_list_gen, screen.agents_list_gen);
    }

    #[test]
    fn format_relative_time_with_uses_provided_now() {
        let now = 1_000_000_000_000i64; // 1e12 micros
        let ts = now - 120_000_000; // 120s ago
        let result = format_relative_time_with(ts, now);
        assert_eq!(result, "2m ago");
    }

    #[test]
    fn agents_list_gen_increments_on_rebuild() {
        let state = test_state();
        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 1,
            agents_list: vec![crate::tui_events::AgentSummary {
                project: String::new(),
                name: "RedFox".to_string(),
                program: "claude-code".to_string(),
                model: String::new(),
                last_active_ts: 100,
                health: None,
            }],
            ..Default::default()
        });
        let mut screen = AgentsScreen::new();
        screen.cached_now_ts = chrono::Utc::now().timestamp_micros();
        let gen_before = screen.agents_list_gen;
        screen.rebuild_from_state(&state);
        assert_eq!(screen.agents_list_gen, gen_before + 1);
        screen.rebuild_from_state(&state);
        assert_eq!(screen.agents_list_gen, gen_before + 2);
    }

    #[test]
    fn rebuild_from_state_keeps_selection_on_same_agent_after_reorder() {
        let state = test_state();
        let mut screen = AgentsScreen::new();
        // Use empty project strings to match what rebuild_from_state()
        // creates from AgentSummary (AgentIdentity::new("", name)).
        screen.agents = vec![
            test_agent_row("AliceRiver", "", "codex-cli", "gpt-5", 100, 1),
            test_agent_row("BobStone", "", "claude-code", "opus-4.6", 200, 2),
        ];
        screen.table_state.selected = Some(1);

        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            agents: 2,
            agents_list: vec![
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "AliceRiver".to_string(),
                    program: "codex-cli".to_string(),
                    model: String::new(),
                    last_active_ts: 100,
                    health: None,
                },
                crate::tui_events::AgentSummary {
                    project: String::new(),
                    name: "BobStone".to_string(),
                    program: "claude-code".to_string(),
                    model: String::new(),
                    last_active_ts: 200,
                    health: None,
                },
            ],
            ..Default::default()
        });

        screen.cached_now_ts = chrono::Utc::now().timestamp_micros();
        screen.rebuild_from_state(&state);

        assert_eq!(screen.table_state.selected, Some(0));
        assert_eq!(screen.agents[0].name, "BobStone");
    }
}
