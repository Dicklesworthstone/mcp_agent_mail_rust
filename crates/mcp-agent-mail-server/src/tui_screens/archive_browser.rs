//! Archive Browser screen — two-pane file browser for the Git-backed archive.
//!
//! Left pane: expandable directory tree showing archive structure.
//! Right pane: file content preview with format-aware rendering
//! (syntax-highlighted JSON, rendered markdown, plain text with line numbers).

use std::path::{Path, PathBuf};

use ftui::layout::{Breakpoint, Constraint, Flex, Rect, ResponsiveLayout};
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEvent, KeyEventKind, Modifiers, PackedRgba, Style};
use ftui_runtime::program::Cmd;

use crate::tui_bridge::TuiSharedState;
use crate::tui_screens::{HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_theme::TuiThemePalette;
use crate::tui_widgets::fancy::SummaryFooter;

const MAX_PREVIEW_BYTES: u64 = 512 * 1024;
const ARCHIVE_SPLIT_GAP_THRESHOLD: u16 = 70;

fn render_splitter_handle(frame: &mut Frame<'_>, area: Rect, active: bool) {
    if area.is_empty() {
        return;
    }
    let tp = TuiThemePalette::current();
    let separator_color = crate::tui_theme::lerp_color(tp.panel_bg, tp.panel_border_dim, 0.58);
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = frame.buffer.get_mut(x, y) {
                *cell = ftui::Cell::from_char(' ');
                cell.fg = separator_color;
                cell.bg = tp.panel_bg;
            }
        }
    }

    if active && area.height >= 5 {
        let x = area.x.saturating_add(area.width.saturating_sub(1) / 2);
        let y = area.y.saturating_add(area.height.saturating_sub(1) / 2);
        if let Some(cell) = frame.buffer.get_mut(x, y) {
            *cell = ftui::Cell::from_char('·');
            cell.fg = tp.selection_indicator;
            cell.bg = tp.panel_bg;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Archive Entry types
// ──────────────────────────────────────────────────────────────────────

/// A file or directory entry in the archive tree.
#[derive(Debug, Clone)]
struct ArchiveEntry {
    /// Display name (just the file/dir name, not full path).
    name: String,
    /// Full path relative to archive root.
    rel_path: PathBuf,
    /// Whether this is a directory.
    is_dir: bool,
    /// File size in bytes (0 for directories).
    size: u64,
    /// Depth in the tree (0 = root level).
    depth: usize,
    /// Whether this directory is expanded.
    expanded: bool,
    /// Number of children (for directories).
    child_count: usize,
}

impl ArchiveEntry {
    fn display_prefix(&self) -> String {
        let indent = "  ".repeat(self.depth);
        let icon = if self.is_dir {
            if self.expanded { "▾ " } else { "▸ " }
        } else {
            "  "
        };
        format!("{indent}{icon}")
    }

    fn display_label(&self) -> String {
        if self.is_dir {
            format!("{}/", self.name)
        } else {
            self.name.clone()
        }
    }

    fn display_size(&self) -> String {
        if self.is_dir {
            if self.child_count > 0 {
                format!("{} items", self.child_count)
            } else {
                String::new()
            }
        } else {
            format_file_size(self.size)
        }
    }
}

/// Format bytes into human-readable size.
fn format_file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        let mut whole = bytes / 1024;
        let mut tenths = ((bytes % 1024) * 10 + 512) / 1024;
        if tenths == 10 {
            whole += 1;
            tenths = 0;
        }
        format!("{whole}.{tenths} KB")
    } else if bytes < 1024 * 1024 * 1024 {
        let divider = 1024 * 1024;
        let mut whole = bytes / divider;
        let mut tenths = ((bytes % divider) * 10 + (divider / 2)) / divider;
        if tenths == 10 {
            whole += 1;
            tenths = 0;
        }
        format!("{whole}.{tenths} MB")
    } else {
        let divider = 1024 * 1024 * 1024;
        let mut whole = bytes / divider;
        let mut hundredths = ((bytes % divider) * 100 + (divider / 2)) / divider;
        if hundredths == 100 {
            whole += 1;
            hundredths = 0;
        }
        format!("{whole}.{hundredths:02} GB")
    }
}

fn truncate_path_label(path: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let tail: String = path.chars().rev().take(max_chars - 1).collect();
    let tail: String = tail.chars().rev().collect();
    format!("…{tail}")
}

/// Detect content type from file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentType {
    Json,
    Markdown,
    Toml,
    Yaml,
    PlainText,
    Binary,
}

impl ContentType {
    fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
            "json" | "jsonl" => Self::Json,
            "md" | "markdown" => Self::Markdown,
            "toml" => Self::Toml,
            "yml" | "yaml" => Self::Yaml,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "bmp" | "sqlite" | "db"
            | "sqlite3" => Self::Binary,
            _ => Self::PlainText,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::Markdown => "Markdown",
            Self::Toml => "TOML",
            Self::Yaml => "YAML",
            Self::PlainText => "Text",
            Self::Binary => "Binary",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Archive Browser Screen
// ──────────────────────────────────────────────────────────────────────

/// Theme-aware file-type color lookup, replacing former hardcoded COLOR_* constants.
const fn file_type_color(ct: ContentType, is_dir: bool, tp: &TuiThemePalette) -> PackedRgba {
    if is_dir {
        tp.metric_agents // blue (was COLOR_DIR)
    } else {
        match ct {
            ContentType::Json => tp.metric_reservations, // yellow (was COLOR_JSON)
            ContentType::Markdown => tp.activity_active, // green (was COLOR_MARKDOWN)
            ContentType::Toml | ContentType::Yaml => tp.severity_warn, // magenta-ish (was COLOR_CONFIG)
            ContentType::Binary => tp.text_muted,                      // gray (was COLOR_DIM)
            ContentType::PlainText => tp.text_primary,
        }
    }
}

/// Two-pane archive browser: directory tree (left) + file preview (right).
pub struct ArchiveBrowserScreen {
    /// Flattened visible tree entries.
    entries: Vec<ArchiveEntry>,
    /// Table state for the directory tree pane.
    tree_state: TableState,
    /// Currently loaded file content (if any).
    preview_content: Option<String>,
    /// Content type of the currently previewed file.
    preview_type: ContentType,
    /// Path of the currently previewed file.
    preview_path: String,
    /// Scroll offset in the preview pane.
    preview_scroll: u16,
    /// Which pane has focus: `false` = tree, `true` = preview.
    preview_focused: bool,
    /// Current archive root path.
    archive_root: Option<PathBuf>,
    /// Filter text for searching file names.
    filter: String,
    /// Whether filter input is active.
    filter_active: bool,
    /// Selected project slug (first project by default).
    selected_project: Option<String>,
    /// Last tick when entries were refreshed.
    last_refresh_tick: u64,
    /// Whether the preview panel is visible on wide screens (user toggle).
    detail_visible: bool,
    /// Generation snapshot from last tick (for dirty-state gating).
    last_data_gen: super::DataGeneration,
}

impl ArchiveBrowserScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            tree_state: TableState::default(),
            preview_content: None,
            preview_type: ContentType::PlainText,
            preview_path: String::new(),
            preview_scroll: 0,
            preview_focused: false,
            archive_root: None,
            filter: String::new(),
            filter_active: false,
            selected_project: None,
            last_refresh_tick: 0,
            detail_visible: true,
            last_data_gen: super::DataGeneration::stale(),
        }
    }

    /// Rebuild the entry list from the archive on disk.
    fn refresh_entries(&mut self, state: &TuiSharedState) {
        let config = state.config_snapshot();
        let storage_root = &config.storage_root;
        if storage_root.is_empty() {
            self.entries.clear();
            return;
        }

        // Find the first project's archive directory.
        let db = state.db_stats_snapshot().unwrap_or_default();
        let project_slug = self
            .selected_project
            .clone()
            .or_else(|| db.projects_list.first().map(|p| p.slug.clone()));

        let Some(slug) = project_slug else {
            self.entries.clear();
            return;
        };
        self.selected_project = Some(slug.clone());

        let archive_path = PathBuf::from(storage_root).join(&slug).join(".archive");
        if archive_path.is_dir() {
            self.archive_root = Some(archive_path);
        } else {
            // Try without .archive suffix (some layouts use project root directly)
            let alt_path = PathBuf::from(storage_root).join(&slug);
            if alt_path.is_dir() {
                self.archive_root = Some(alt_path);
            } else {
                self.archive_root = None;
                self.entries.clear();
                return;
            }
        }

        // Build flattened visible tree
        let root = self.archive_root.as_ref().unwrap();
        let mut entries = Vec::new();
        Self::scan_directory(root, root, 0, &self.filter, &mut entries);
        self.entries = entries;

        // Clamp selection
        if let Some(sel) = self.tree_state.selected
            && sel >= self.entries.len()
        {
            self.tree_state.selected = if self.entries.is_empty() {
                None
            } else {
                Some(self.entries.len() - 1)
            };
        }
    }

    /// Recursively scan a directory and add entries to the flat list.
    fn scan_directory(
        root: &Path,
        dir: &Path,
        depth: usize,
        filter: &str,
        entries: &mut Vec<ArchiveEntry>,
    ) {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return;
        };

        let mut items: Vec<(bool, String, PathBuf, u64)> = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip hidden files/dirs (like .git)
            if name.starts_with('.') {
                continue;
            }

            let is_dir = path.is_dir();
            let size = if is_dir {
                0
            } else {
                entry.metadata().map_or(0, |metadata| metadata.len())
            };

            // Apply filter (only to files; always show dirs that might contain matches)
            if !filter.is_empty() && !is_dir {
                let f = filter.to_lowercase();
                if !name.to_lowercase().contains(&f) {
                    continue;
                }
            }

            items.push((is_dir, name, path, size));
        }

        // Sort: directories first, then alphabetically
        items.sort_by(|a, b| match (a.0, b.0) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.1.to_lowercase().cmp(&b.1.to_lowercase()),
        });

        for (is_dir, name, path, size) in items {
            let rel_path = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            let child_count = if is_dir {
                std::fs::read_dir(&path).map_or(0, std::iter::Iterator::count)
            } else {
                0
            };

            // Find if this dir was previously expanded
            let expanded = entries
                .iter()
                .find(|e| e.rel_path == rel_path)
                .is_some_and(|e| e.expanded);

            entries.push(ArchiveEntry {
                name,
                rel_path,
                is_dir,
                size,
                depth,
                expanded,
                child_count,
            });

            // If directory is expanded, recurse
            if is_dir && expanded {
                Self::scan_directory(root, &path, depth + 1, filter, entries);
            }
        }
    }

    /// Load file content for preview.
    fn load_preview(&mut self) {
        let Some(sel) = self.tree_state.selected else {
            self.preview_content = None;
            return;
        };
        let Some(entry) = self.entries.get(sel) else {
            self.preview_content = None;
            return;
        };

        if entry.is_dir {
            self.preview_content = None;
            self.preview_path = entry.rel_path.display().to_string();
            self.preview_type = ContentType::PlainText;
            return;
        }

        let Some(root) = &self.archive_root else {
            self.preview_content = None;
            return;
        };

        let full_path = root.join(&entry.rel_path);
        self.preview_path = entry.rel_path.display().to_string();
        self.preview_type = ContentType::from_path(&full_path);
        self.preview_scroll = 0;

        if self.preview_type == ContentType::Binary {
            self.preview_content = Some(format!(
                "[Binary file: {} — {}]",
                entry.name,
                format_file_size(entry.size)
            ));
            return;
        }

        // Read file content with size limit (max 512 KB)
        if entry.size > MAX_PREVIEW_BYTES {
            self.preview_content = Some(format!(
                "[File too large for preview: {} — {}]\n\nShowing first {} of content...",
                entry.name,
                format_file_size(entry.size),
                format_file_size(MAX_PREVIEW_BYTES)
            ));
            // Read partial
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                let max_chars = usize::try_from(MAX_PREVIEW_BYTES).unwrap_or(usize::MAX);
                let truncated: String = content.chars().take(max_chars).collect();
                self.preview_content = Some(truncated);
            }
            return;
        }

        match std::fs::read_to_string(&full_path) {
            Ok(content) => {
                self.preview_content = Some(content);
            }
            Err(e) => {
                self.preview_content = Some(format!("[Error reading file: {e}]"));
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len();
        let current = self.tree_state.selected.unwrap_or(0);
        let next = if delta > 0 {
            current.saturating_add(delta.unsigned_abs()).min(len - 1)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.tree_state.selected = Some(next);
    }

    fn toggle_expand(&mut self) {
        let Some(sel) = self.tree_state.selected else {
            return;
        };
        if let Some(entry) = self.entries.get_mut(sel)
            && entry.is_dir
        {
            entry.expanded = !entry.expanded;
            // Re-scan to rebuild visible entries with expansion state
            if let Some(root) = &self.archive_root {
                let root = root.clone();
                let filter = self.filter.clone();
                // Preserve expansion states
                let expanded_paths: Vec<(PathBuf, bool)> = self
                    .entries
                    .iter()
                    .filter(|e| e.is_dir)
                    .map(|e| (e.rel_path.clone(), e.expanded))
                    .collect();

                // Re-scan with correct expansion
                let mut final_entries = Vec::new();
                Self::scan_directory_with_state(
                    &root,
                    &root,
                    0,
                    &filter,
                    &expanded_paths,
                    &mut final_entries,
                );
                self.entries = final_entries;

                // Clamp selection
                if let Some(s) = self.tree_state.selected
                    && s >= self.entries.len()
                    && !self.entries.is_empty()
                {
                    self.tree_state.selected = Some(self.entries.len() - 1);
                }
            }
        }
    }

    /// Scan directory with pre-set expansion state.
    fn scan_directory_with_state(
        root: &Path,
        dir: &Path,
        depth: usize,
        filter: &str,
        expanded_state: &[(PathBuf, bool)],
        entries: &mut Vec<ArchiveEntry>,
    ) {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return;
        };

        let mut items: Vec<(bool, String, PathBuf, u64)> = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if name.starts_with('.') {
                continue;
            }

            let is_dir = path.is_dir();
            let size = if is_dir {
                0
            } else {
                entry.metadata().map_or(0, |metadata| metadata.len())
            };

            if !filter.is_empty()
                && !is_dir
                && !name.to_lowercase().contains(&filter.to_lowercase())
            {
                continue;
            }

            items.push((is_dir, name, path, size));
        }

        items.sort_by(|a, b| match (a.0, b.0) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.1.to_lowercase().cmp(&b.1.to_lowercase()),
        });

        for (is_dir, name, path, size) in items {
            let rel_path = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            let child_count = if is_dir {
                std::fs::read_dir(&path).map_or(0, std::iter::Iterator::count)
            } else {
                0
            };

            let expanded = is_dir
                && expanded_state
                    .iter()
                    .any(|(p, expanded)| *p == rel_path && *expanded);

            entries.push(ArchiveEntry {
                name,
                rel_path,
                is_dir,
                size,
                depth,
                expanded,
                child_count,
            });

            if is_dir && expanded {
                Self::scan_directory_with_state(
                    root,
                    &path,
                    depth + 1,
                    filter,
                    expanded_state,
                    entries,
                );
            }
        }
    }

    /// Render the directory tree pane.
    fn render_tree(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        let tp = crate::tui_theme::TuiThemePalette::current();

        let border_style = if self.preview_focused {
            Style::default().fg(tp.panel_border_dim)
        } else {
            Style::default().fg(tp.panel_border_focused)
        };

        let project_label = self.selected_project.as_deref().unwrap_or("(no project)");
        let tree_title = format!(" Archive: {project_label} ");
        let block = Block::bordered()
            .title(tree_title.as_str())
            .border_type(BorderType::Rounded)
            .border_style(border_style);

        let inner = block.inner(area);
        block.render(area, frame);

        if self.entries.is_empty() {
            let (title, hint) = if self.archive_root.is_none() {
                (
                    "No Archive Found",
                    "The Git archive is created when the first message is sent. Check `am doctor` for diagnostics.",
                )
            } else if !self.filter.is_empty() {
                (
                    "No Matches",
                    "No files match the current filter. Press Esc to clear.",
                )
            } else {
                ("Empty Archive", "Archive directory contains no files.")
            };
            crate::tui_panel_helpers::render_empty_state(frame, inner, "\u{1f4c1}", title, hint);
            return;
        }

        // Build table rows from entries
        let rows: Vec<Row> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let prefix = entry.display_prefix();
                let label = entry.display_label();
                let size = entry.display_size();

                let ct = ContentType::from_path(&entry.rel_path);
                let fg = file_type_color(ct, entry.is_dir, &tp);
                let name_style = if entry.is_dir {
                    Style::default().fg(fg).bold()
                } else {
                    Style::default().fg(fg)
                };

                let selected = self.tree_state.selected == Some(i);
                let row_style = if selected && !self.preview_focused {
                    Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
                } else {
                    name_style
                };

                Row::new(vec![format!("{prefix}{label}"), size]).style(row_style)
            })
            .collect();

        let widths = [Constraint::Min(20), Constraint::Fixed(12)];

        let table = Table::new(rows, widths);

        let mut ts = self.tree_state.clone();
        StatefulWidget::render(&table, inner, frame, &mut ts);
    }

    /// Render the file preview pane.
    fn render_preview(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();

        let border_style = if self.preview_focused {
            Style::default().fg(tp.panel_border_focused)
        } else {
            Style::default().fg(tp.panel_border_dim)
        };

        let type_label = self.preview_type.label();
        let title = if self.preview_path.is_empty() {
            " Preview ".to_string()
        } else {
            let type_label_chars = u16::try_from(type_label.chars().count()).unwrap_or(u16::MAX);
            let max_path_chars =
                area.width
                    .saturating_sub(type_label_chars.saturating_add(8)) as usize;
            let path_label = truncate_path_label(&self.preview_path, max_path_chars.max(4));
            format!(" {path_label} [{type_label}] ")
        };

        let block = Block::bordered()
            .title(title.as_str())
            .border_type(BorderType::Rounded)
            .border_style(border_style);

        let inner = block.inner(area);
        block.render(area, frame);
        let content_inner = if inner.width > 2 {
            Rect::new(
                inner.x.saturating_add(1),
                inner.y,
                inner.width.saturating_sub(2),
                inner.height,
            )
        } else {
            inner
        };
        if content_inner.width == 0 || content_inner.height == 0 {
            return;
        }

        let content = self.preview_content.as_deref().unwrap_or(
            "Select a file to preview its contents.\n\n\
             Navigation:\n\
             \u{2022} Arrow keys / j/k to navigate the tree\n\
             \u{2022} Enter to expand/collapse directories or select files\n\
             \u{2022} Tab to switch between tree and preview panes\n\
             \u{2022} / to search file names\n\
             \u{2022} Esc to clear filter",
        );

        // Add line numbers for non-markdown, non-binary content
        let display_text = if self.preview_content.is_some()
            && self.preview_type != ContentType::Binary
            && self.preview_type != ContentType::Markdown
        {
            add_line_numbers(content)
        } else {
            content.to_string()
        };

        if self.preview_type == ContentType::Markdown && self.preview_content.is_some() {
            let md_theme = crate::tui_theme::markdown_theme();
            let text = crate::tui_markdown::render_body(content, &md_theme);
            let tp = crate::tui_theme::TuiThemePalette::current();
            Paragraph::new(text)
                .scroll((self.preview_scroll, 0))
                .style(crate::tui_theme::text_primary(&tp))
                .render(content_inner, frame);
        } else {
            let tp = crate::tui_theme::TuiThemePalette::current();
            Paragraph::new(display_text)
                .scroll((self.preview_scroll, 0))
                .style(crate::tui_theme::text_primary(&tp))
                .render(content_inner, frame);
        }
    }

    fn handle_filter_key(
        &mut self,
        key_code: KeyCode,
        state: &TuiSharedState,
    ) -> Cmd<MailScreenMsg> {
        match key_code {
            KeyCode::Escape => {
                self.filter_active = false;
                self.filter.clear();
                self.refresh_entries(state);
            }
            KeyCode::Enter => {
                self.filter_active = false;
                self.refresh_entries(state);
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.refresh_entries(state);
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.refresh_entries(state);
            }
            _ => {}
        }
        Cmd::None
    }

    fn handle_preview_key(&mut self, key: &KeyEvent) -> Cmd<MailScreenMsg> {
        match key.code {
            KeyCode::Tab | KeyCode::Escape => {
                self.preview_focused = false;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.preview_scroll = self.preview_scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.preview_scroll = self.preview_scroll.saturating_sub(1);
            }
            KeyCode::Char('d') if key.modifiers.contains(Modifiers::CTRL) => {
                self.preview_scroll = self.preview_scroll.saturating_add(20);
            }
            KeyCode::Char('u') if key.modifiers.contains(Modifiers::CTRL) => {
                self.preview_scroll = self.preview_scroll.saturating_sub(20);
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.preview_scroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                if let Some(content) = &self.preview_content {
                    let line_count = content.lines().count();
                    let clamped = line_count.saturating_sub(10);
                    self.preview_scroll = u16::try_from(clamped).unwrap_or(u16::MAX);
                }
            }
            _ => {}
        }
        Cmd::None
    }

    fn handle_tree_key(&mut self, key_code: KeyCode, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        match key_code {
            KeyCode::Char('i') => {
                self.detail_visible = !self.detail_visible;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                self.load_preview();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                self.load_preview();
            }
            KeyCode::PageDown => {
                self.move_selection(20);
                self.load_preview();
            }
            KeyCode::PageUp => {
                self.move_selection(-20);
                self.load_preview();
            }
            KeyCode::Home | KeyCode::Char('g') => {
                if !self.entries.is_empty() {
                    self.tree_state.selected = Some(0);
                    self.load_preview();
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.entries.is_empty() {
                    self.tree_state.selected = Some(self.entries.len() - 1);
                    self.load_preview();
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(sel) = self.tree_state.selected
                    && let Some(entry) = self.entries.get(sel)
                {
                    if entry.is_dir {
                        self.toggle_expand();
                    } else {
                        self.load_preview();
                        self.preview_focused = true;
                    }
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(sel) = self.tree_state.selected
                    && let Some(entry) = self.entries.get(sel)
                {
                    if entry.is_dir && entry.expanded {
                        self.toggle_expand();
                    } else if entry.depth > 0 {
                        for i in (0..sel).rev() {
                            if self.entries[i].is_dir && self.entries[i].depth < entry.depth {
                                self.tree_state.selected = Some(i);
                                self.load_preview();
                                break;
                            }
                        }
                    }
                }
            }
            KeyCode::Tab => {
                self.preview_focused = true;
            }
            KeyCode::Char('/') => {
                self.filter_active = true;
            }
            KeyCode::Char('r') => {
                self.refresh_entries(state);
            }
            _ => {}
        }
        Cmd::None
    }
}

impl ArchiveBrowserScreen {
    /// Compute summary stats: `(file_count, dir_count, total_bytes)`.
    fn archive_stats(&self) -> (u64, u64, u64) {
        let files = self.entries.iter().filter(|e| !e.is_dir).count() as u64;
        let dirs = self.entries.iter().filter(|e| e.is_dir).count() as u64;
        let total_bytes: u64 = self.entries.iter().map(|e| e.size).sum();
        (files, dirs, total_bytes)
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = TuiThemePalette::current();
        let (files, dirs, total_bytes) = self.archive_stats();

        let files_str = files.to_string();
        let dirs_str = dirs.to_string();
        let size_str = format_file_size(total_bytes);

        let items: Vec<(&str, &str, PackedRgba)> = vec![
            (&*files_str, "files", tp.metric_messages),
            (&*dirs_str, "dirs", tp.metric_agents),
            (&*size_str, "total", tp.metric_latency),
        ];

        SummaryFooter::new(&items, tp.text_muted).render(area, frame);
    }
}

/// Add line numbers to text content.
fn add_line_numbers(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let width = lines.len().to_string().len();
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>width$} \u{2502} {}", i + 1, line, width = width))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Default for ArchiveBrowserScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for ArchiveBrowserScreen {
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            if self.filter_active {
                return self.handle_filter_key(key.code, state);
            }
            if self.preview_focused {
                return self.handle_preview_key(key);
            }
            return self.handle_tree_key(key.code, state);
        }
        Cmd::None
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        // Layout: [tree | preview](dynamic) + footer(1)
        let footer_h: u16 = u16::from(area.height >= 6);
        let content_h = area.height.saturating_sub(footer_h);
        let content_area = Rect::new(area.x, area.y, area.width, content_h);

        // Responsive tree/preview split using ResponsiveLayout breakpoints.
        // Xs–Sm (< 90): tree only. Md (90–119): 45/55. Lg (120–159): 40/60. Xl (160+): 35/65.
        let layout = if self.detail_visible {
            ResponsiveLayout::new(Flex::vertical().constraints([Constraint::Fill]))
                .at(
                    Breakpoint::Md,
                    Flex::horizontal()
                        .constraints([Constraint::Percentage(45.0), Constraint::Percentage(55.0)]),
                )
                .at(
                    Breakpoint::Lg,
                    Flex::horizontal()
                        .constraints([Constraint::Percentage(40.0), Constraint::Percentage(60.0)]),
                )
                .at(
                    Breakpoint::Xl,
                    Flex::horizontal()
                        .constraints([Constraint::Percentage(35.0), Constraint::Percentage(65.0)]),
                )
        } else {
            ResponsiveLayout::new(Flex::vertical().constraints([Constraint::Fill]))
        };

        let split = layout.split(content_area);
        let tree_area = split.rects[0];
        let has_preview = split.rects.len() >= 2;

        // Render tree pane (with optional filter bar)
        if self.filter_active {
            let tree_chunks = Flex::vertical()
                .constraints([Constraint::Min(3), Constraint::Fixed(3)])
                .split(tree_area);

            self.render_tree(frame, tree_chunks[0], state);

            let tp = TuiThemePalette::current();
            let filter_block = Block::bordered()
                .title(" Filter: ")
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(tp.metric_reservations));
            let inner = filter_block.inner(tree_chunks[1]);
            filter_block.render(tree_chunks[1], frame);
            let filter_text = Paragraph::new(format!("{}\u{258e}", self.filter));
            filter_text.render(inner, frame);
        } else {
            self.render_tree(frame, tree_area, state);
        }

        // Render preview pane (Md+)
        if has_preview {
            let mut preview_area = split.rects[1];
            let split_gap = u16::from(content_area.width >= ARCHIVE_SPLIT_GAP_THRESHOLD);
            if split_gap > 0 && preview_area.width > split_gap {
                preview_area.x = preview_area.x.saturating_add(split_gap);
                preview_area.width = preview_area.width.saturating_sub(split_gap);
                let splitter_area = Rect::new(
                    tree_area.x.saturating_add(tree_area.width),
                    content_area.y,
                    split_gap,
                    content_area.height,
                );
                render_splitter_handle(frame, splitter_area, false);
            }
            self.render_preview(frame, preview_area);
        }

        // ── Footer summary ─────────────────────────────────────────────
        if footer_h > 0 {
            let footer_area = Rect::new(area.x, area.y + content_h, area.width, footer_h);
            self.render_footer(frame, footer_area);
        }
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        let current_gen = state.data_generation();
        let dirty = super::dirty_since(&self.last_data_gen, &current_gen);

        // First tick: always initialize (ungated).
        let is_first = self.last_refresh_tick == 0;
        // Periodic refresh every 50 ticks (~5 s), but only when data changed.
        let is_periodic = tick_count.saturating_sub(self.last_refresh_tick) > 50 && dirty.any();

        if is_first || is_periodic {
            self.last_refresh_tick = tick_count;
            // Preserve expansion state across refreshes
            let expanded_state: Vec<(PathBuf, bool)> = self
                .entries
                .iter()
                .filter(|e| e.is_dir)
                .map(|e| (e.rel_path.clone(), e.expanded))
                .collect();

            self.refresh_entries(state);

            // Restore expansion state
            for entry in &mut self.entries {
                if entry.is_dir
                    && let Some((_, exp)) =
                        expanded_state.iter().find(|(p, _)| *p == entry.rel_path)
                {
                    entry.expanded = *exp;
                }
            }
        }

        self.last_data_gen = current_gen;
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Navigate tree",
            },
            HelpEntry {
                key: "Enter",
                action: "Expand dir / preview file",
            },
            HelpEntry {
                key: "h",
                action: "Collapse dir / go to parent",
            },
            HelpEntry {
                key: "Tab",
                action: "Switch pane focus",
            },
            HelpEntry {
                key: "/",
                action: "Filter files by name",
            },
            HelpEntry {
                key: "r",
                action: "Refresh",
            },
            HelpEntry {
                key: "g/G",
                action: "Jump to top/bottom",
            },
            HelpEntry {
                key: "Ctrl+D/U",
                action: "Page down/up (preview)",
            },
            HelpEntry {
                key: "i",
                action: "Toggle preview panel",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some(
            "Browse the Git-backed message archive. Navigate directories with arrow keys, preview files with Enter.",
        )
    }

    fn consumes_text_input(&self) -> bool {
        self.filter_active
    }

    fn copyable_content(&self) -> Option<String> {
        if self.preview_focused {
            self.preview_content.clone()
        } else {
            self.tree_state.selected.and_then(|sel| {
                self.entries
                    .get(sel)
                    .map(|e| e.rel_path.display().to_string())
            })
        }
    }

    fn title(&self) -> &'static str {
        "Archive Browser"
    }

    fn tab_label(&self) -> &'static str {
        "Archive"
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_file_size() {
        assert_eq!(format_file_size(0), "0 B");
        assert_eq!(format_file_size(512), "512 B");
        assert_eq!(format_file_size(1024), "1.0 KB");
        assert_eq!(format_file_size(1536), "1.5 KB");
        assert_eq!(format_file_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_file_size(1024 * 1024 * 1024), "1.00 GB");
    }

    #[test]
    fn test_content_type_detection() {
        assert_eq!(
            ContentType::from_path(Path::new("test.json")),
            ContentType::Json
        );
        assert_eq!(
            ContentType::from_path(Path::new("test.jsonl")),
            ContentType::Json
        );
        assert_eq!(
            ContentType::from_path(Path::new("readme.md")),
            ContentType::Markdown
        );
        assert_eq!(
            ContentType::from_path(Path::new("config.toml")),
            ContentType::Toml
        );
        assert_eq!(
            ContentType::from_path(Path::new("data.yml")),
            ContentType::Yaml
        );
        assert_eq!(
            ContentType::from_path(Path::new("image.png")),
            ContentType::Binary
        );
        assert_eq!(
            ContentType::from_path(Path::new("notes.txt")),
            ContentType::PlainText
        );
        assert_eq!(
            ContentType::from_path(Path::new("unknown")),
            ContentType::PlainText
        );
    }

    #[test]
    fn test_add_line_numbers() {
        let text = "line one\nline two\nline three";
        let numbered = add_line_numbers(text);
        assert!(numbered.contains("1 \u{2502} line one"));
        assert!(numbered.contains("2 \u{2502} line two"));
        assert!(numbered.contains("3 \u{2502} line three"));
    }

    #[test]
    fn test_archive_entry_display() {
        let dir_entry = ArchiveEntry {
            name: "messages".to_string(),
            rel_path: PathBuf::from("messages"),
            is_dir: true,
            size: 0,
            depth: 0,
            expanded: true,
            child_count: 5,
        };
        assert_eq!(dir_entry.display_label(), "messages/");
        assert!(dir_entry.display_prefix().contains('\u{25be}'));
        assert_eq!(dir_entry.display_size(), "5 items");

        let file_entry = ArchiveEntry {
            name: "inbox.json".to_string(),
            rel_path: PathBuf::from("messages/inbox.json"),
            is_dir: false,
            size: 2048,
            depth: 1,
            expanded: false,
            child_count: 0,
        };
        assert_eq!(file_entry.display_label(), "inbox.json");
        assert_eq!(file_entry.display_size(), "2.0 KB");
    }

    #[test]
    fn test_archive_entry_collapsed_dir() {
        let dir_entry = ArchiveEntry {
            name: "agents".to_string(),
            rel_path: PathBuf::from("agents"),
            is_dir: true,
            size: 0,
            depth: 0,
            expanded: false,
            child_count: 3,
        };
        assert!(dir_entry.display_prefix().contains('\u{25b8}'));
    }

    #[test]
    fn test_content_type_labels() {
        assert_eq!(ContentType::Json.label(), "JSON");
        assert_eq!(ContentType::Markdown.label(), "Markdown");
        assert_eq!(ContentType::Toml.label(), "TOML");
        assert_eq!(ContentType::Yaml.label(), "YAML");
        assert_eq!(ContentType::PlainText.label(), "Text");
        assert_eq!(ContentType::Binary.label(), "Binary");
    }

    #[test]
    fn test_screen_defaults() {
        let screen = ArchiveBrowserScreen::new();
        assert!(screen.entries.is_empty());
        assert!(!screen.preview_focused);
        assert!(!screen.filter_active);
        assert!(screen.preview_content.is_none());
        assert_eq!(screen.title(), "Archive Browser");
        assert_eq!(screen.tab_label(), "Archive");
    }

    #[test]
    fn test_move_selection_empty() {
        let mut screen = ArchiveBrowserScreen::new();
        screen.move_selection(1);
        assert_eq!(screen.tree_state.selected, None);
    }

    #[test]
    fn test_move_selection_bounds() {
        let mut screen = ArchiveBrowserScreen::new();
        screen.entries = vec![
            ArchiveEntry {
                name: "a".into(),
                rel_path: "a".into(),
                is_dir: false,
                size: 0,
                depth: 0,
                expanded: false,
                child_count: 0,
            },
            ArchiveEntry {
                name: "b".into(),
                rel_path: "b".into(),
                is_dir: false,
                size: 0,
                depth: 0,
                expanded: false,
                child_count: 0,
            },
            ArchiveEntry {
                name: "c".into(),
                rel_path: "c".into(),
                is_dir: false,
                size: 0,
                depth: 0,
                expanded: false,
                child_count: 0,
            },
        ];
        screen.tree_state.selected = Some(0);

        // Move down
        screen.move_selection(1);
        assert_eq!(screen.tree_state.selected, Some(1));

        // Move down past end - should clamp
        screen.move_selection(100);
        assert_eq!(screen.tree_state.selected, Some(2));

        // Move up past start - should clamp to 0
        screen.move_selection(-100);
        assert_eq!(screen.tree_state.selected, Some(0));
    }

    #[test]
    fn test_keybindings_non_empty() {
        let screen = ArchiveBrowserScreen::new();
        let bindings = screen.keybindings();
        assert!(!bindings.is_empty());
        assert!(bindings.iter().any(|b| b.key.contains("Enter")));
        assert!(bindings.iter().any(|b| b.key.contains("Tab")));
    }

    #[test]
    fn test_scan_directory_with_tempdir() {
        let tmp = std::env::temp_dir().join("archive_browser_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("messages/2026/01")).unwrap();
        std::fs::write(tmp.join("messages/2026/01/msg1.md"), "# Hello").unwrap();
        std::fs::write(tmp.join("messages/2026/01/msg2.json"), "{}").unwrap();
        std::fs::create_dir_all(tmp.join("agents/RedFox")).unwrap();
        std::fs::write(
            tmp.join("agents/RedFox/profile.json"),
            "{\"name\":\"RedFox\"}",
        )
        .unwrap();

        let mut entries = Vec::new();
        ArchiveBrowserScreen::scan_directory(&tmp, &tmp, 0, "", &mut entries);

        // Should have agents/ and messages/ at depth 0
        assert!(entries.len() >= 2);
        assert!(entries.iter().any(|e| e.name == "agents" && e.is_dir));
        assert!(entries.iter().any(|e| e.name == "messages" && e.is_dir));

        // Dirs should be sorted first
        let first_file_idx = entries.iter().position(|e| !e.is_dir);
        let last_dir_idx = entries.iter().rposition(|e| e.is_dir);
        if let (Some(ff), Some(ld)) = (first_file_idx, last_dir_idx) {
            assert!(ld < ff, "dirs should come before files");
        }

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn truncate_path_label_short_path_unchanged() {
        assert_eq!(truncate_path_label("foo/bar.rs", 20), "foo/bar.rs");
    }

    #[test]
    fn truncate_path_label_exact_length_unchanged() {
        assert_eq!(truncate_path_label("abcde", 5), "abcde");
    }

    #[test]
    fn truncate_path_label_truncates_from_left() {
        let result = truncate_path_label("very/long/path/to/file.rs", 10);
        assert!(result.starts_with('…'));
        assert_eq!(result.chars().count(), 10);
        assert!(result.ends_with("o/file.rs"));
    }

    #[test]
    fn truncate_path_label_max_zero_returns_empty() {
        assert_eq!(truncate_path_label("anything", 0), "");
    }

    #[test]
    fn truncate_path_label_max_one_returns_ellipsis() {
        assert_eq!(truncate_path_label("anything", 1), "…");
    }

    #[test]
    fn truncate_path_label_unicode_counts_chars_not_bytes() {
        // 4 chars, but > 4 bytes in UTF-8
        let result = truncate_path_label("日本語テスト", 4);
        assert!(result.starts_with('…'));
        assert_eq!(result.chars().count(), 4);
    }

    #[test]
    fn format_file_size_boundary_values() {
        assert_eq!(format_file_size(1023), "1023 B");
        assert_eq!(format_file_size(1024), "1.0 KB");
        assert_eq!(format_file_size(1024 * 1024 - 1), "1024.0 KB");
        assert_eq!(format_file_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_file_size(1024 * 1024 * 1024 - 1), "1024.0 MB");
        assert_eq!(format_file_size(1024 * 1024 * 1024), "1.00 GB");
    }

    #[test]
    fn add_line_numbers_empty_string() {
        let result = add_line_numbers("");
        // "".lines() returns an empty iterator, so the result is empty
        assert!(result.is_empty());
    }

    #[test]
    fn add_line_numbers_single_line() {
        let result = add_line_numbers("hello");
        assert!(result.contains("1 \u{2502} hello"));
        assert!(!result.contains("2 \u{2502}"));
    }

    #[test]
    fn test_scan_directory_with_filter() {
        let tmp = std::env::temp_dir().join("archive_browser_filter_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("readme.md"), "# README").unwrap();
        std::fs::write(tmp.join("config.json"), "{}").unwrap();
        std::fs::write(tmp.join("notes.txt"), "notes").unwrap();

        let mut entries = Vec::new();
        ArchiveBrowserScreen::scan_directory(&tmp, &tmp, 0, "json", &mut entries);

        // Only json file should match
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "config.json");

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
