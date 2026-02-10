//! Contacts screen — cross-agent contact links and policy display.

use ftui::layout::Constraint;
use ftui::layout::Rect;
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba, Style};
use ftui_runtime::program::Cmd;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::ContactSummary;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};

/// Column indices for sorting.
const COL_FROM: usize = 0;
const COL_TO: usize = 1;
const COL_STATUS: usize = 2;
const COL_REASON: usize = 3;
const COL_UPDATED: usize = 4;

const SORT_LABELS: &[&str] = &["From", "To", "Status", "Reason", "Updated"];

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
                    KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
                    KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
                    KeyCode::Char('G') | KeyCode::End => {
                        if !self.contacts.is_empty() {
                            self.table_state.selected = Some(self.contacts.len() - 1);
                        }
                    }
                    KeyCode::Char('g') | KeyCode::Home => {
                        if !self.contacts.is_empty() {
                            self.table_state.selected = Some(0);
                        }
                    }
                    KeyCode::Char('/') => {
                        self.filter_active = true;
                        self.filter.clear();
                    }
                    KeyCode::Char('f') => {
                        self.status_filter = self.status_filter.next();
                        self.rebuild_from_state(state);
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
                        if !self.filter.is_empty() {
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

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
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
                    Style::default()
                        .fg(PackedRgba::rgb(0, 0, 0))
                        .bg(PackedRgba::rgb(180, 140, 220))
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
            .border_type(BorderType::Rounded);

        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(
                Style::default()
                    .fg(PackedRgba::rgb(0, 0, 0))
                    .bg(PackedRgba::rgb(180, 140, 220)),
            );

        let mut ts = self.table_state.clone();
        StatefulWidget::render(&table, table_area, frame, &mut ts);
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Select contact",
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
                key: "s",
                action: "Cycle sort column",
            },
            HelpEntry {
                key: "S",
                action: "Toggle sort order",
            },
            HelpEntry {
                key: "Esc",
                action: "Clear filter",
            },
        ]
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

    fn title(&self) -> &'static str {
        "Contacts"
    }

    fn tab_label(&self) -> &'static str {
        "Links"
    }
}

/// Color style based on contact status.
fn status_color(status: &str) -> Style {
    match status {
        "approved" => Style::default().fg(PackedRgba::rgb(80, 200, 120)),
        "pending" => Style::default().fg(PackedRgba::rgb(220, 200, 60)),
        "blocked" => Style::default().fg(PackedRgba::rgb(220, 80, 80)),
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

/// Truncate a string to `max_len`, adding "..." suffix if needed.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len < 4 {
        "...".to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
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
}
