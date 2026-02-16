//! Contacts screen — cross-agent contact links and policy display.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use ftui::layout::Constraint;
use ftui::layout::Rect;
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Buffer, Event, Frame, KeyCode, KeyEventKind, Style};
use ftui_extras::canvas::{CanvasRef, Mode, Painter};
use ftui_extras::mermaid::{self, MermaidCompatibilityMatrix, MermaidFallbackPolicy};
use ftui_extras::{mermaid_layout, mermaid_render};
use ftui_runtime::program::Cmd;

use crate::tui_action_menu::{ActionEntry, contacts_actions};
use crate::tui_bridge::TuiSharedState;
use crate::tui_events::{ContactSummary, MailEvent};
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_widgets::generate_contact_graph_mermaid;

/// Column indices for sorting.
const COL_FROM: usize = 0;
const COL_TO: usize = 1;
const COL_STATUS: usize = 2;
const COL_REASON: usize = 3;
const COL_UPDATED: usize = 4;

const SORT_LABELS: &[&str] = &["From", "To", "Status", "Reason", "Updated"];
const MERMAID_RENDER_DEBOUNCE: Duration = Duration::from_secs(1);
const GRAPH_EVENTS_WINDOW: usize = 512;
const GRAPH_MIN_WIDTH: u16 = 60;
const GRAPH_MIN_HEIGHT: u16 = 10;

#[derive(Debug, Clone)]
struct MermaidPanelCache {
    source_hash: u64,
    width: u16,
    height: u16,
    buffer: Buffer,
}

#[derive(Debug, Default, Clone)]
struct GraphFlowMetrics {
    edge_volume: HashMap<(String, String), u32>,
    node_sent: HashMap<String, u32>,
    node_received: HashMap<String, u32>,
}

impl GraphFlowMetrics {
    fn node_total(&self, agent: &str) -> u32 {
        self.node_sent.get(agent).copied().unwrap_or(0)
            + self.node_received.get(agent).copied().unwrap_or(0)
    }

    fn edge_weight(&self, from: &str, to: &str) -> u32 {
        self.edge_volume
            .get(&(from.to_string(), to.to_string()))
            .copied()
            .unwrap_or(0)
    }

    fn max_node_total(&self) -> u32 {
        self.node_sent
            .keys()
            .chain(self.node_received.keys())
            .map(|agent| self.node_total(agent))
            .max()
            .unwrap_or(0)
    }

    fn max_edge_weight(&self) -> u32 {
        self.edge_volume.values().copied().max().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Table,
    Graph,
}

/// Status filter modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    All,
    Pending,
    Approved,
    Blocked,
}

impl StatusFilter {
    const fn next(self) -> Self {
        match self {
            Self::All => Self::Pending,
            Self::Pending => Self::Approved,
            Self::Approved => Self::Blocked,
            Self::Blocked => Self::All,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Pending => "Pending",
            Self::Approved => "Approved",
            Self::Blocked => "Blocked",
        }
    }

    fn matches(self, status: &str) -> bool {
        match self {
            Self::All => true,
            Self::Pending => status == "pending",
            Self::Approved => status == "approved",
            Self::Blocked => status == "blocked",
        }
    }
}

pub struct ContactsScreen {
    table_state: TableState,
    contacts: Vec<ContactSummary>,
    sort_col: usize,
    sort_asc: bool,
    filter: String,
    filter_active: bool,
    status_filter: StatusFilter,
    view_mode: ViewMode,
    /// (Agent Name, x, y) normalized 0.0-1.0
    graph_nodes: Vec<(String, f64, f64)>,
    graph_selected_idx: usize,
    show_mermaid_panel: bool,
    mermaid_cache: RefCell<Option<MermaidPanelCache>>,
    mermaid_last_render_at: RefCell<Option<Instant>>,
}

impl ContactsScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            table_state: TableState::default(),
            contacts: Vec::new(),
            sort_col: COL_UPDATED,
            sort_asc: false,
            filter: String::new(),
            filter_active: false,
            status_filter: StatusFilter::All,
            view_mode: ViewMode::Table,
            graph_nodes: Vec::new(),
            graph_selected_idx: 0,
            show_mermaid_panel: false,
            mermaid_cache: RefCell::new(None),
            mermaid_last_render_at: RefCell::new(None),
        }
    }

    fn rebuild_from_state(&mut self, state: &TuiSharedState) {
        let db = state.db_stats_snapshot().unwrap_or_default();
        let mut rows: Vec<ContactSummary> = db.contacts_list;

        // Apply status filter
        let sf = self.status_filter;
        rows.retain(|r| sf.matches(&r.status));

        // Apply text filter
        if !self.filter.is_empty() {
            let f = self.filter.to_lowercase();
            rows.retain(|r| {
                r.from_agent.to_lowercase().contains(&f)
                    || r.to_agent.to_lowercase().contains(&f)
                    || r.reason.to_lowercase().contains(&f)
                    || r.from_project_slug.to_lowercase().contains(&f)
            });
        }

        // Sort
        rows.sort_by(|a, b| {
            let cmp = match self.sort_col {
                COL_FROM => a
                    .from_agent
                    .to_lowercase()
                    .cmp(&b.from_agent.to_lowercase()),
                COL_TO => a.to_agent.to_lowercase().cmp(&b.to_agent.to_lowercase()),
                COL_STATUS => a.status.cmp(&b.status),
                COL_REASON => a.reason.to_lowercase().cmp(&b.reason.to_lowercase()),
                COL_UPDATED => a.updated_ts.cmp(&b.updated_ts),
                _ => std::cmp::Ordering::Equal,
            };
            if self.sort_asc { cmp } else { cmp.reverse() }
        });

        self.contacts = rows;
        let recent_events = state.recent_events(GRAPH_EVENTS_WINDOW);
        self.layout_graph(&recent_events);

        // Clamp selection
        if let Some(sel) = self.table_state.selected {
            if sel >= self.contacts.len() {
                self.table_state.selected = if self.contacts.is_empty() {
                    None
                } else {
                    Some(self.contacts.len() - 1)
                };
            }
        }
    }

    fn layout_graph(&mut self, recent_events: &[MailEvent]) {
        // Collect unique agents
        let mut agents = std::collections::HashSet::new();
        for c in &self.contacts {
            agents.insert(c.from_agent.clone());
            agents.insert(c.to_agent.clone());
        }
        for (from, recipients) in message_flow_iter(recent_events) {
            if !from.trim().is_empty() {
                agents.insert(from.to_string());
            }
            for to in recipients {
                if !to.trim().is_empty() {
                    agents.insert(to.to_string());
                }
            }
        }
        let mut agents_vec: Vec<String> = agents.into_iter().collect();
        agents_vec.sort();

        let count = agents_vec.len();
        self.graph_nodes.clear();
        self.graph_selected_idx = self.graph_selected_idx.min(count.saturating_sub(1));

        if count == 0 {
            return;
        }

        // Circle layout
        for (i, agent) in agents_vec.into_iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let angle = 2.0 * std::f64::consts::PI * (i as f64) / (count as f64);
            // Center at 0.5, 0.5; radius 0.4
            let x = 0.4f64.mul_add(angle.cos(), 0.5);
            let y = 0.4f64.mul_add(angle.sin(), 0.5);
            self.graph_nodes.push((agent, x, y));
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.contacts.is_empty() {
            return;
        }
        let len = self.contacts.len();
        let current = self.table_state.selected.unwrap_or(0);
        let next = if delta > 0 {
            current.saturating_add(delta.unsigned_abs()).min(len - 1)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.table_state.selected = Some(next);
    }

    fn move_graph_selection(&mut self, delta: isize) {
        if self.graph_nodes.is_empty() {
            self.graph_selected_idx = 0;
            return;
        }
        let len = self.graph_nodes.len();
        let current = self.graph_selected_idx;
        let next = if delta > 0 {
            current
                .saturating_add(delta.unsigned_abs())
                .min(len.saturating_sub(1))
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.graph_selected_idx = next;
    }

    fn selected_graph_agent(&self) -> Option<&str> {
        self.graph_nodes
            .get(self.graph_selected_idx)
            .map(|(name, _, _)| name.as_str())
    }
}

impl Default for ContactsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for ContactsScreen {
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
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
                    KeyCode::Char('j') | KeyCode::Down => {
                        if self.view_mode == ViewMode::Graph && !self.show_mermaid_panel {
                            self.move_graph_selection(1);
                        } else {
                            self.move_selection(1);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if self.view_mode == ViewMode::Graph && !self.show_mermaid_panel {
                            self.move_graph_selection(-1);
                        } else {
                            self.move_selection(-1);
                        }
                    }
                    KeyCode::Left => {
                        if self.view_mode == ViewMode::Graph && !self.show_mermaid_panel {
                            self.move_graph_selection(-1);
                        }
                    }
                    KeyCode::Right => {
                        if self.view_mode == ViewMode::Graph && !self.show_mermaid_panel {
                            self.move_graph_selection(1);
                        }
                    }
                    KeyCode::Char('G') | KeyCode::End => {
                        if self.view_mode == ViewMode::Graph && !self.show_mermaid_panel {
                            self.graph_selected_idx = self.graph_nodes.len().saturating_sub(1);
                        } else if !self.contacts.is_empty() {
                            self.table_state.selected = Some(self.contacts.len() - 1);
                        }
                    }
                    KeyCode::Home => {
                        if self.view_mode == ViewMode::Graph && !self.show_mermaid_panel {
                            self.graph_selected_idx = 0;
                        } else if !self.contacts.is_empty() {
                            self.table_state.selected = Some(0);
                        }
                    }
                    KeyCode::Enter => {
                        if self.view_mode == ViewMode::Graph && !self.show_mermaid_panel {
                            if let Some(agent) = self.selected_graph_agent() {
                                return Cmd::msg(MailScreenMsg::DeepLink(
                                    DeepLinkTarget::AgentByName(agent.to_string()),
                                ));
                            }
                        }
                    }
                    KeyCode::Char('g') => {
                        self.show_mermaid_panel = !self.show_mermaid_panel;
                    }
                    KeyCode::Char('/') => {
                        self.filter_active = true;
                        self.filter.clear();
                    }
                    KeyCode::Char('f') => {
                        self.status_filter = self.status_filter.next();
                        self.rebuild_from_state(state);
                    }
                    KeyCode::Char('n') => {
                        self.view_mode = match self.view_mode {
                            ViewMode::Table => ViewMode::Graph,
                            ViewMode::Graph => ViewMode::Table,
                        };
                        self.graph_selected_idx = 0;
                    }
                    KeyCode::Char('s') => {
                        self.sort_col = (self.sort_col + 1) % SORT_LABELS.len();
                        self.rebuild_from_state(state);
                    }
                    KeyCode::Char('S') => {
                        self.sort_asc = !self.sort_asc;
                        self.rebuild_from_state(state);
                    }
                    KeyCode::Escape => {
                        if self.show_mermaid_panel {
                            self.show_mermaid_panel = false;
                        } else if !self.filter.is_empty() {
                            self.filter.clear();
                            self.rebuild_from_state(state);
                        }
                    }
                    _ => {}
                }
            }
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        // Rebuild every 5 seconds (contacts change infrequently)
        if tick_count % 50 == 0 {
            self.rebuild_from_state(state);
        }
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        if area.height < 3 || area.width < 20 {
            return;
        }

        let header_h = 1_u16;
        let table_h = area.height.saturating_sub(header_h);

        let header_area = Rect::new(area.x, area.y, area.width, header_h);
        let table_area = Rect::new(area.x, area.y + header_h, area.width, table_h);

        // Render header info line
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
            "{} contacts | Status: {} | Sort: {}{} {}",
            self.contacts.len(),
            self.status_filter.label(),
            sort_label,
            sort_indicator,
            filter_display,
        );
        let p = Paragraph::new(info);
        p.render(header_area, frame);

        let graph_mode_active = self.view_mode == ViewMode::Graph || self.show_mermaid_panel;
        let recent_events = if graph_mode_active {
            state.recent_events(GRAPH_EVENTS_WINDOW)
        } else {
            Vec::new()
        };
        let metrics = build_graph_flow_metrics(&self.contacts, &recent_events);

        if self.show_mermaid_panel {
            self.render_mermaid_panel(frame, table_area, &recent_events);
        } else if self.view_mode == ViewMode::Graph
            && table_area.width >= GRAPH_MIN_WIDTH
            && table_area.height >= GRAPH_MIN_HEIGHT
        {
            self.render_graph(frame, table_area, &metrics);
        } else {
            self.render_table(frame, table_area);
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Select contact / graph node",
            },
            HelpEntry {
                key: "g",
                action: "Toggle Mermaid graph panel",
            },
            HelpEntry {
                key: "Enter",
                action: "Open selected graph node in Agents",
            },
            HelpEntry {
                key: "/",
                action: "Search/filter",
            },
            HelpEntry {
                key: "f",
                action: "Cycle status filter",
            },
            HelpEntry {
                key: "n",
                action: "Toggle Table/Graph",
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
                key: "Esc",
                action: "Close Mermaid / clear filter",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("Agent contact links and approval status. Accept/deny pending requests.")
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        if let DeepLinkTarget::ContactByPair(from, to) = target {
            if let Some(pos) = self
                .contacts
                .iter()
                .position(|c| c.from_agent == *from && c.to_agent == *to)
            {
                self.table_state.selected = Some(pos);
                return true;
            }
        }
        false
    }

    fn consumes_text_input(&self) -> bool {
        self.filter_active
    }

    fn contextual_actions(&self) -> Option<(Vec<ActionEntry>, u16, String)> {
        let selected_idx = self.table_state.selected?;
        let contact = self.contacts.get(selected_idx)?;

        let actions = contacts_actions(&contact.from_agent, &contact.to_agent, &contact.status);

        // Anchor row is the selected row + header offset
        #[allow(clippy::cast_possible_truncation)]
        let anchor_row = (selected_idx as u16).saturating_add(2);
        let context_id = format!("{}:{}", contact.from_agent, contact.to_agent);

        Some((actions, anchor_row, context_id))
    }

    fn copyable_content(&self) -> Option<String> {
        let idx = self.table_state.selected?;
        let contact = self.contacts.get(idx)?;
        Some(format!(
            "{} -> {} ({})",
            contact.from_agent, contact.to_agent, contact.status
        ))
    }

    fn title(&self) -> &'static str {
        "Contacts"
    }

    fn tab_label(&self) -> &'static str {
        "Links"
    }
}

// Helper methods for ContactsScreen (not part of MailScreen trait)
impl ContactsScreen {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn render_graph(&self, frame: &mut Frame<'_>, area: Rect, metrics: &GraphFlowMetrics) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = Block::default()
            .title("Network Graph")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let inner = block.inner(area);
        block.render(area, frame);

        if inner.width < 4 || inner.height < 4 {
            return;
        }

        let mut painter = Painter::for_area(inner, Mode::Braille);
        painter.clear();

        let w = f64::from(inner.width) * 2.0; // Braille resolution width (2 cols per cell)
        let h = f64::from(inner.height) * 4.0; // Braille resolution height (4 rows per cell)
        let max_edge_weight = metrics.max_edge_weight();

        // Draw edges with directional arrowheads and weight-based thickness.
        for contact in &self.contacts {
            if let (Some(start), Some(end)) = (
                self.find_node(&contact.from_agent),
                self.find_node(&contact.to_agent),
            ) {
                let color = match contact.status.as_str() {
                    "approved" => tp.contact_approved,
                    "blocked" => tp.contact_blocked,
                    _ => tp.contact_pending,
                };

                let x1 = (start.1 * w).round() as i32;
                let y1 = (start.2 * h).round() as i32;
                let x2 = (end.1 * w).round() as i32;
                let y2 = (end.2 * h).round() as i32;

                let weight = metrics.edge_weight(&contact.from_agent, &contact.to_agent);
                let thickness = scaled_level(weight, max_edge_weight, 1, 3) as i32;
                draw_weighted_line(&mut painter, x1, y1, x2, y2, thickness, color);
                draw_arrow_head(&mut painter, x1, y1, x2, y2, color);
            }
        }

        // Draw nodes with traffic-based radius and selected highlight.
        let max_node_volume = metrics.max_node_total();
        for (idx, (name, nx, ny)) in self.graph_nodes.iter().enumerate() {
            let x = (nx * w).round() as i32;
            let y = (ny * h).round() as i32;
            let node_volume = metrics.node_total(name);
            let radius = scaled_level(node_volume, max_node_volume, 1, 3) as i32;
            let selected = idx == self.graph_selected_idx;
            let node_color = if selected {
                tp.panel_border_focused
            } else {
                tp.text_primary
            };
            for dx in -radius..=radius {
                for dy in -radius..=radius {
                    if dx * dx + dy * dy <= radius * radius {
                        painter.point_colored(x + dx, y + dy, node_color);
                    }
                }
            }
        }

        CanvasRef::from_painter(&painter).render(inner, frame);

        // Draw labels (overlay on top of canvas)
        for (idx, (name, nx, ny)) in self.graph_nodes.iter().enumerate() {
            // Map normalized coords back to cell coords
            let cx = inner.x + (nx * f64::from(inner.width)) as u16;
            let cy = inner.y + (ny * f64::from(inner.height)) as u16;

            // Simple centering logic
            let label: String = name.chars().take(8).collect();
            let lx = cx.saturating_sub(label.len() as u16 / 2);

            if lx >= inner.x
                && lx + label.len() as u16 <= inner.right()
                && cy >= inner.y
                && cy < inner.bottom()
            {
                let selected = idx == self.graph_selected_idx;
                let fg_color = if selected {
                    tp.selection_fg
                } else {
                    tp.panel_title_fg
                };
                let bg_color = if selected {
                    tp.selection_bg
                } else {
                    tp.bg_deep
                };
                for (i, ch) in label.chars().enumerate() {
                    if let Some(cell) = frame.buffer.get_mut(lx + i as u16, cy) {
                        cell.content = ftui::Cell::from_char(ch).content;
                        cell.fg = fg_color;
                        cell.bg = bg_color;
                    }
                }
            }
        }

        if let Some(agent) = self.selected_graph_agent() {
            let sent = metrics.node_sent.get(agent).copied().unwrap_or(0);
            let received = metrics.node_received.get(agent).copied().unwrap_or(0);
            let total = sent + received;
            let hint = format!(
                "Node: {agent} | sent: {sent} recv: {received} total: {total} | Enter: open agent"
            );
            let hint_rect = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
            Paragraph::new(hint).render(hint_rect, frame);
        }
    }

    fn render_mermaid_panel(&self, frame: &mut Frame<'_>, area: Rect, events: &[MailEvent]) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = Block::default()
            .title("Mermaid Contact Graph [g]")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let inner = block.inner(area);
        block.render(area, frame);

        if inner.width < 4 || inner.height < 4 {
            return;
        }

        let source = generate_contact_graph_mermaid(&self.contacts, events);
        let source_hash = stable_hash(source.as_bytes());

        let (has_cache, source_changed, size_changed) = {
            let cache = self.mermaid_cache.borrow();
            cache.as_ref().map_or((false, true, true), |cached| {
                (
                    true,
                    cached.source_hash != source_hash,
                    cached.width != inner.width || cached.height != inner.height,
                )
            })
        };
        let cache_is_fresh = has_cache && !source_changed && !size_changed;

        let can_refresh = self
            .mermaid_last_render_at
            .borrow()
            .as_ref()
            .is_none_or(|last| last.elapsed() >= MERMAID_RENDER_DEBOUNCE);

        // Refresh immediately when source/size changes; debounce only protects
        // against redundant refresh attempts for unchanged content.
        if !cache_is_fresh && (!has_cache || source_changed || size_changed || can_refresh) {
            let buffer = render_mermaid_source_to_buffer(&source, inner.width, inner.height);
            *self.mermaid_cache.borrow_mut() = Some(MermaidPanelCache {
                source_hash,
                width: inner.width,
                height: inner.height,
                buffer,
            });
            *self.mermaid_last_render_at.borrow_mut() = Some(Instant::now());
        }

        if let Some(cache) = self.mermaid_cache.borrow().as_ref() {
            blit_buffer_to_frame(frame, inner, &cache.buffer);
        } else {
            Paragraph::new("Preparing Mermaid graph...").render(inner, frame);
        }
    }

    fn find_node(&self, name: &str) -> Option<&(String, f64, f64)> {
        self.graph_nodes.iter().find(|n| n.0 == name)
    }

    fn render_table(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        // Build table rows
        let header = Row::new(["From", "To", "Status", "Reason", "Updated", "Expires"])
            .style(Style::default().bold());

        let rows: Vec<Row> = self
            .contacts
            .iter()
            .enumerate()
            .map(|(i, contact)| {
                let updated_str = format_relative_ts(contact.updated_ts);
                let expires_str = contact
                    .expires_ts
                    .map_or_else(|| "never".to_string(), format_relative_ts);
                let status_style = status_color(&contact.status);
                let row_style = if Some(i) == self.table_state.selected {
                    Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
                } else {
                    status_style
                };
                Row::new([
                    contact.from_agent.clone(),
                    contact.to_agent.clone(),
                    contact.status.clone(),
                    truncate_str(&contact.reason, 20),
                    updated_str,
                    expires_str,
                ])
                .style(row_style)
            })
            .collect();

        let widths = [
            Constraint::Percentage(18.0),
            Constraint::Percentage(18.0),
            Constraint::Percentage(12.0),
            Constraint::Percentage(22.0),
            Constraint::Percentage(15.0),
            Constraint::Percentage(15.0),
        ];

        let block = Block::default()
            .title("Contacts")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));

        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));

        let mut ts = self.table_state.clone();
        StatefulWidget::render(&table, area, frame, &mut ts);
    }
}

fn build_graph_flow_metrics(contacts: &[ContactSummary], events: &[MailEvent]) -> GraphFlowMetrics {
    let mut metrics = GraphFlowMetrics::default();
    for contact in contacts {
        metrics
            .edge_volume
            .entry((contact.from_agent.clone(), contact.to_agent.clone()))
            .or_insert(0);
    }

    for (from, recipients) in message_flow_iter(events) {
        if from.trim().is_empty() {
            continue;
        }
        for to in recipients {
            if to.trim().is_empty() {
                continue;
            }
            *metrics
                .edge_volume
                .entry((from.to_string(), to.to_string()))
                .or_insert(0) += 1;
            *metrics.node_sent.entry(from.to_string()).or_insert(0) += 1;
            *metrics.node_received.entry(to.to_string()).or_insert(0) += 1;
        }
    }

    if metrics.max_edge_weight() == 0 {
        for contact in contacts {
            *metrics
                .edge_volume
                .entry((contact.from_agent.clone(), contact.to_agent.clone()))
                .or_insert(0) += 1;
            *metrics
                .node_sent
                .entry(contact.from_agent.clone())
                .or_insert(0) += 1;
            *metrics
                .node_received
                .entry(contact.to_agent.clone())
                .or_insert(0) += 1;
        }
    }

    metrics
}

fn message_flow_iter(events: &[MailEvent]) -> impl Iterator<Item = (&str, &[String])> {
    events.iter().filter_map(|event| match event {
        MailEvent::MessageSent { from, to, .. } | MailEvent::MessageReceived { from, to, .. } => {
            Some((from.as_str(), to.as_slice()))
        }
        _ => None,
    })
}

fn scaled_level(value: u32, max_value: u32, min: u32, max: u32) -> u32 {
    if min >= max || max_value == 0 {
        return min;
    }
    let clamped = value.min(max_value);
    let range = max - min;
    let scaled = min + (clamped.saturating_mul(range) + (max_value / 2)) / max_value;
    scaled.clamp(min, max)
}

fn draw_weighted_line(
    painter: &mut Painter,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    thickness: i32,
    color: ftui::PackedRgba,
) {
    painter.line_colored(x1, y1, x2, y2, Some(color));
    if thickness <= 1 {
        return;
    }

    let dx = (x2 - x1).abs();
    let dy = (y2 - y1).abs();
    if dx >= dy {
        painter.line_colored(x1, y1 + 1, x2, y2 + 1, Some(color));
        if thickness >= 3 {
            painter.line_colored(x1, y1 - 1, x2, y2 - 1, Some(color));
        }
    } else {
        painter.line_colored(x1 + 1, y1, x2 + 1, y2, Some(color));
        if thickness >= 3 {
            painter.line_colored(x1 - 1, y1, x2 - 1, y2, Some(color));
        }
    }
}

fn draw_arrow_head(
    painter: &mut Painter,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    color: ftui::PackedRgba,
) {
    let vx = f64::from(x2 - x1);
    let vy = f64::from(y2 - y1);
    let len = (vx * vx + vy * vy).sqrt();
    if len < 1.0 {
        return;
    }
    let ux = vx / len;
    let uy = vy / len;
    let arrow_len = 3.0;
    let wing = 1.6;
    let base_x = f64::from(x2) - ux * arrow_len;
    let base_y = f64::from(y2) - uy * arrow_len;
    let perp_x = -uy;
    let perp_y = ux;
    let left_x = (base_x + perp_x * wing).round() as i32;
    let left_y = (base_y + perp_y * wing).round() as i32;
    let right_x = (base_x - perp_x * wing).round() as i32;
    let right_y = (base_y - perp_y * wing).round() as i32;
    painter.line_colored(x2, y2, left_x, left_y, Some(color));
    painter.line_colored(x2, y2, right_x, right_y, Some(color));
}

fn stable_hash<T: Hash>(value: T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn render_mermaid_source_to_buffer(source: &str, width: u16, height: u16) -> Buffer {
    let mut buffer = Buffer::new(width, height);
    let config = mermaid::MermaidConfig::from_env();
    if !config.enabled {
        for (idx, ch) in "Mermaid disabled via env".chars().enumerate() {
            if let Ok(x) = u16::try_from(idx) {
                if x >= width {
                    break;
                }
                buffer.set(x, 0, ftui::Cell::from_char(ch));
            } else {
                break;
            }
        }
        return buffer;
    }

    let matrix = MermaidCompatibilityMatrix::default();
    let policy = MermaidFallbackPolicy::default();
    let parsed = mermaid::parse_with_diagnostics(source);
    let ir_parse = mermaid::normalize_ast_to_ir(&parsed.ast, &config, &matrix, &policy);
    let mut errors = parsed.errors;
    errors.extend(ir_parse.errors);

    let render_area = Rect::from_size(width, height);
    let layout = mermaid_layout::layout_diagram(&ir_parse.ir, &config);
    let _plan = mermaid_render::render_diagram_adaptive(
        &layout,
        &ir_parse.ir,
        &config,
        render_area,
        &mut buffer,
    );

    if !errors.is_empty() {
        let has_content = !ir_parse.ir.nodes.is_empty()
            || !ir_parse.ir.edges.is_empty()
            || !ir_parse.ir.labels.is_empty()
            || !ir_parse.ir.clusters.is_empty();
        if has_content {
            mermaid_render::render_mermaid_error_overlay(
                &errors,
                source,
                &config,
                render_area,
                &mut buffer,
            );
        } else {
            mermaid_render::render_mermaid_error_panel(
                &errors,
                source,
                &config,
                render_area,
                &mut buffer,
            );
        }
    }

    buffer
}

fn blit_buffer_to_frame(frame: &mut Frame<'_>, area: Rect, buffer: &Buffer) {
    let width = area.width.min(buffer.width());
    let height = area.height.min(buffer.height());
    for y in 0..height {
        for x in 0..width {
            let Some(src) = buffer.get(x, y) else {
                continue;
            };
            let dst_x = area.x + x;
            let dst_y = area.y + y;
            if let Some(dst) = frame.buffer.get_mut(dst_x, dst_y) {
                *dst = *src;
            }
        }
    }
}

/// Color style based on contact status.
fn status_color(status: &str) -> Style {
    let tp = crate::tui_theme::TuiThemePalette::current();
    match status {
        "approved" => Style::default().fg(tp.contact_approved),
        "pending" => Style::default().fg(tp.contact_pending),
        "blocked" => Style::default().fg(tp.contact_blocked),
        _ => Style::default(),
    }
}

/// Format a microsecond timestamp as relative time.
fn format_relative_ts(ts_micros: i64) -> String {
    if ts_micros == 0 {
        return "never".to_string();
    }
    let now = chrono::Utc::now().timestamp_micros();
    let delta_secs = (now - ts_micros) / 1_000_000;
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

/// Truncate a string to `max_len` characters, adding "..." suffix if needed.
fn truncate_str(s: &str, max_len: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_len {
        s.to_string()
    } else if max_len < 4 {
        "...".to_string()
    } else {
        let truncated: String = s.chars().take(max_len - 3).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_core::Config;

    fn test_state() -> std::sync::Arc<TuiSharedState> {
        TuiSharedState::new(&Config::default())
    }

    #[test]
    fn new_screen_has_defaults() {
        let screen = ContactsScreen::new();
        assert!(screen.contacts.is_empty());
        assert!(!screen.filter_active);
        assert_eq!(screen.sort_col, COL_UPDATED);
        assert!(!screen.sort_asc);
        assert_eq!(screen.status_filter, StatusFilter::All);
    }

    #[test]
    fn renders_without_panic() {
        let state = test_state();
        let screen = ContactsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn renders_at_minimum_size() {
        let state = test_state();
        let screen = ContactsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(20, 3, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 20, 3), &state);
    }

    #[test]
    fn renders_at_tiny_size_without_panic() {
        let state = test_state();
        let screen = ContactsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(10, 2, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 10, 2), &state);
    }

    #[test]
    fn title_and_label() {
        let screen = ContactsScreen::new();
        assert_eq!(screen.title(), "Contacts");
        assert_eq!(screen.tab_label(), "Links");
    }

    #[test]
    fn keybindings_documented() {
        let screen = ContactsScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.len() >= 5);
        assert!(bindings.iter().any(|b| b.key == "j/k"));
        assert!(bindings.iter().any(|b| b.key == "g"));
        assert!(bindings.iter().any(|b| b.key == "f"));
    }

    #[test]
    fn slash_activates_filter() {
        let state = test_state();
        let mut screen = ContactsScreen::new();
        assert!(!screen.consumes_text_input());

        let slash = Event::Key(ftui::KeyEvent::new(KeyCode::Char('/')));
        screen.update(&slash, &state);
        assert!(screen.consumes_text_input());
    }

    #[test]
    fn f_cycles_status_filter() {
        let state = test_state();
        let mut screen = ContactsScreen::new();
        assert_eq!(screen.status_filter, StatusFilter::All);

        let f = Event::Key(ftui::KeyEvent::new(KeyCode::Char('f')));
        screen.update(&f, &state);
        assert_eq!(screen.status_filter, StatusFilter::Pending);

        screen.update(&f, &state);
        assert_eq!(screen.status_filter, StatusFilter::Approved);

        screen.update(&f, &state);
        assert_eq!(screen.status_filter, StatusFilter::Blocked);

        screen.update(&f, &state);
        assert_eq!(screen.status_filter, StatusFilter::All);
    }

    #[test]
    fn s_cycles_sort_column() {
        let state = test_state();
        let mut screen = ContactsScreen::new();
        let initial = screen.sort_col;

        let s = Event::Key(ftui::KeyEvent::new(KeyCode::Char('s')));
        screen.update(&s, &state);
        assert_ne!(screen.sort_col, initial);
    }

    #[test]
    fn deep_link_contact_by_pair() {
        let mut screen = ContactsScreen::new();
        screen.contacts.push(ContactSummary {
            from_agent: "GoldFox".into(),
            to_agent: "RedWolf".into(),
            status: "approved".into(),
            ..Default::default()
        });
        let handled = screen.receive_deep_link(&DeepLinkTarget::ContactByPair(
            "GoldFox".into(),
            "RedWolf".into(),
        ));
        assert!(handled);
        assert_eq!(screen.table_state.selected, Some(0));
    }

    #[test]
    fn deep_link_unknown_contact() {
        let mut screen = ContactsScreen::new();
        let handled =
            screen.receive_deep_link(&DeepLinkTarget::ContactByPair("X".into(), "Y".into()));
        assert!(!handled);
    }

    #[test]
    fn status_filter_matches() {
        assert!(StatusFilter::All.matches("approved"));
        assert!(StatusFilter::All.matches("pending"));
        assert!(StatusFilter::Pending.matches("pending"));
        assert!(!StatusFilter::Pending.matches("approved"));
        assert!(StatusFilter::Approved.matches("approved"));
        assert!(!StatusFilter::Approved.matches("blocked"));
        assert!(StatusFilter::Blocked.matches("blocked"));
    }

    #[test]
    fn format_relative_ts_values() {
        assert_eq!(format_relative_ts(0), "never");
        let now = chrono::Utc::now().timestamp_micros();
        let result = format_relative_ts(now - 30_000_000);
        assert!(result.contains("s ago"));
    }

    #[test]
    fn truncate_str_values() {
        assert_eq!(truncate_str("short", 20), "short");
        assert_eq!(truncate_str("this is a long reason", 10), "this is...");
        assert_eq!(truncate_str("abc", 3), "abc"); // fits exactly
        assert_eq!(truncate_str("abcd", 3), "..."); // max_len < 4 → "..."
    }

    #[test]
    fn default_impl() {
        let screen = ContactsScreen::default();
        assert!(screen.contacts.is_empty());
    }

    #[test]
    fn status_color_values() {
        let _ = status_color("approved");
        let _ = status_color("pending");
        let _ = status_color("blocked");
        let _ = status_color("unknown");
    }

    #[test]
    fn move_selection_navigation() {
        let mut screen = ContactsScreen::new();
        screen.contacts.push(ContactSummary::default());
        screen.contacts.push(ContactSummary::default());
        screen.table_state.selected = Some(0);

        screen.move_selection(1);
        assert_eq!(screen.table_state.selected, Some(1));

        screen.move_selection(-1);
        assert_eq!(screen.table_state.selected, Some(0));
    }

    #[test]
    fn g_toggles_mermaid_panel_and_home_keeps_jump_to_start() {
        let state = test_state();
        let mut screen = ContactsScreen::new();
        screen.contacts = vec![ContactSummary::default(), ContactSummary::default()];
        screen.table_state.selected = Some(1);

        let g = Event::Key(ftui::KeyEvent::new(KeyCode::Char('g')));
        screen.update(&g, &state);
        assert!(screen.show_mermaid_panel);
        screen.update(&g, &state);
        assert!(!screen.show_mermaid_panel);

        let home = Event::Key(ftui::KeyEvent::new(KeyCode::Home));
        screen.update(&home, &state);
        assert_eq!(screen.table_state.selected, Some(0));
    }

    #[test]
    fn escape_closes_mermaid_panel_before_clearing_filter() {
        let state = test_state();
        let mut screen = ContactsScreen::new();
        screen.filter = "fox".to_string();
        screen.show_mermaid_panel = true;

        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        screen.update(&esc, &state);

        assert!(!screen.show_mermaid_panel);
        assert_eq!(screen.filter, "fox");
    }

    #[test]
    fn mermaid_panel_render_no_panic() {
        let state = test_state();
        let mut screen = ContactsScreen::new();
        screen.contacts.push(ContactSummary {
            from_agent: "Alpha".to_string(),
            to_agent: "Beta".to_string(),
            status: "approved".to_string(),
            ..Default::default()
        });
        screen.show_mermaid_panel = true;

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(100, 24, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 100, 24), &state);
    }

    #[test]
    fn mermaid_cache_refreshes_immediately_when_source_changes() {
        let state = test_state();
        let mut screen = ContactsScreen::new();
        screen.show_mermaid_panel = true;
        screen.contacts.push(ContactSummary {
            from_agent: "Alpha".to_string(),
            to_agent: "Beta".to_string(),
            status: "approved".to_string(),
            ..Default::default()
        });

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(100, 24, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 100, 24), &state);
        let first_hash = screen
            .mermaid_cache
            .borrow()
            .as_ref()
            .map(|cached| cached.source_hash)
            .expect("first render should populate cache");

        screen.contacts.push(ContactSummary {
            from_agent: "Gamma".to_string(),
            to_agent: "Delta".to_string(),
            status: "approved".to_string(),
            ..Default::default()
        });
        *screen.mermaid_last_render_at.borrow_mut() = Some(Instant::now());

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(100, 24, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 100, 24), &state);
        let second_hash = screen
            .mermaid_cache
            .borrow()
            .as_ref()
            .map(|cached| cached.source_hash)
            .expect("second render should keep cache");

        assert_ne!(first_hash, second_hash);
    }

    #[test]
    fn graph_metrics_track_flow_counts_from_mail_events() {
        let contacts = vec![
            ContactSummary {
                from_agent: "Alpha".to_string(),
                to_agent: "Beta".to_string(),
                status: "approved".to_string(),
                ..Default::default()
            },
            ContactSummary {
                from_agent: "Alpha".to_string(),
                to_agent: "Gamma".to_string(),
                status: "approved".to_string(),
                ..Default::default()
            },
        ];
        let events = vec![
            MailEvent::message_sent(
                1,
                "Alpha",
                vec!["Beta".to_string(), "Gamma".to_string()],
                "s",
                "t",
                "p",
            ),
            MailEvent::message_received(2, "Beta", vec!["Alpha".to_string()], "s", "t", "p"),
        ];
        let metrics = build_graph_flow_metrics(&contacts, &events);
        assert_eq!(metrics.edge_weight("Alpha", "Beta"), 1);
        assert_eq!(metrics.edge_weight("Alpha", "Gamma"), 1);
        assert_eq!(metrics.edge_weight("Beta", "Alpha"), 1);
        assert_eq!(metrics.node_total("Alpha"), 3);
        assert_eq!(metrics.node_total("Gamma"), 1);
    }

    #[test]
    fn graph_metrics_fallback_to_contact_degree_when_no_message_events() {
        let contacts = vec![ContactSummary {
            from_agent: "Alpha".to_string(),
            to_agent: "Beta".to_string(),
            status: "approved".to_string(),
            ..Default::default()
        }];
        let metrics = build_graph_flow_metrics(&contacts, &[]);
        assert_eq!(metrics.edge_weight("Alpha", "Beta"), 1);
        assert_eq!(metrics.node_sent.get("Alpha").copied(), Some(1));
        assert_eq!(metrics.node_received.get("Beta").copied(), Some(1));
    }

    #[test]
    fn graph_layout_includes_agents_seen_only_in_recent_events() {
        let state = test_state();
        let mut screen = ContactsScreen::new();
        state.push_event(MailEvent::message_sent(
            1,
            "OnlyInEvents",
            vec!["Beta".to_string()],
            "subject",
            "thread",
            "project",
        ));
        screen.rebuild_from_state(&state);
        assert!(
            screen
                .graph_nodes
                .iter()
                .any(|(name, _, _)| name == "OnlyInEvents")
        );
    }

    #[test]
    fn enter_on_graph_node_opens_agents_deeplink() {
        let state = test_state();
        let mut screen = ContactsScreen::new();
        screen.view_mode = ViewMode::Graph;
        state.push_event(MailEvent::message_sent(
            1,
            "GraphAgent",
            vec!["Peer".to_string()],
            "subject",
            "thread",
            "project",
        ));
        screen.rebuild_from_state(&state);
        screen.graph_selected_idx = 0;

        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        let cmd = screen.update(&enter, &state);
        assert!(matches!(
            cmd,
            Cmd::Msg(MailScreenMsg::DeepLink(DeepLinkTarget::AgentByName(ref name)))
                if name == "GraphAgent"
        ));
    }

    // ── truncate_str UTF-8 safety ────────────────────────────────────

    #[test]
    fn truncate_str_ascii_short() {
        assert_eq!(truncate_str("hi", 10), "hi");
    }

    #[test]
    fn truncate_str_ascii_over() {
        assert_eq!(truncate_str("hello world!", 8), "hello...");
    }

    #[test]
    fn truncate_str_tiny_max() {
        assert_eq!(truncate_str("hello", 2), "...");
    }

    #[test]
    fn truncate_str_3byte_arrow_no_panic() {
        let s = "foo → bar → baz";
        let r = truncate_str(s, 8);
        assert!(r.chars().count() <= 8);
        assert!(r.ends_with("..."));
    }

    #[test]
    fn truncate_str_cjk_no_panic() {
        let s = "日本語テスト文字列";
        let r = truncate_str(s, 6);
        assert!(r.chars().count() <= 6);
        assert!(r.ends_with("..."));
    }

    #[test]
    fn truncate_str_emoji_no_panic() {
        let s = "🔥🚀💡🎯🏆";
        let r = truncate_str(s, 5);
        assert!(r.chars().count() <= 5);
    }

    #[test]
    fn truncate_str_mixed_multibyte_sweep() {
        let s = "a→b🔥cé";
        for max in 1..=s.chars().count() + 2 {
            let r = truncate_str(s, max);
            assert!(r.chars().count() <= max.max(3), "max={max}");
        }
    }
}
