//! Advanced composable widgets for the TUI operations console.
//!
//! Eight reusable widgets designed for signal density and low render overhead:
//!
//! - [`HeatmapGrid`]: 2D colored cell grid with configurable gradient
//! - [`PercentileRibbon`]: p50/p95/p99 latency bands over time
//! - [`Leaderboard`]: Ranked list with change indicators and delta values
//! - [`AnomalyCard`]: Compact anomaly alert card with severity/confidence badges
//! - [`BrailleActivity`]: Braille-resolution activity sparkline chart
//! - [`MetricTile`]: Compact metric display with inline sparkline
//! - [`ReservationGauge`]: Reservation pressure gauge bar
//! - [`AgentHeatmap`]: Agent-to-agent communication frequency grid
//!
//! Cross-cutting concerns (br-3vwi.6.3):
//!
//! - [`DrillDownAction`] / [`DrillDownWidget`]: keyboard drill-down to navigate into widget data
//! - [`A11yConfig`]: accessibility settings (high contrast, reduced motion, focus visibility)
//! - [`AnimationBudget`]: frame-budget enforcement for animation guardrails

#![forbid(unsafe_code)]

use ftui::layout::Rect;
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Cell, Frame, PackedRgba, Style};
use ftui_extras::canvas::{CanvasRef, Mode, Painter};
use ftui_extras::charts::heatmap_gradient;

// ═══════════════════════════════════════════════════════════════════════════════
// WidgetState — loading / empty / error / ready state envelope
// ═══════════════════════════════════════════════════════════════════════════════

/// State envelope that all advanced widgets can use to render non-data states.
///
/// When the widget has no data yet (loading), has been given an empty dataset,
/// or encountered an error, it renders a descriptive placeholder instead of the
/// normal visualization.
#[derive(Debug, Clone)]
pub enum WidgetState<'a, W> {
    /// Data is being fetched or computed.
    Loading {
        /// Short operator-visible message (e.g., "Fetching metrics...").
        message: &'a str,
    },
    /// Data source returned zero rows.
    Empty {
        /// Operator-visible context (e.g., "No tool calls in the last 5 minutes").
        message: &'a str,
    },
    /// Data source returned an error.
    Error {
        /// Operator-visible error context.
        message: &'a str,
    },
    /// Normal rendering with valid data.
    Ready(W),
}

impl<W: Widget> Widget for WidgetState<'_, W> {
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() {
            return;
        }
        match self {
            Self::Loading { message } => {
                render_state_placeholder(area, frame, "\u{23F3}", message, PackedRgba::rgb(120, 160, 220));
            }
            Self::Empty { message } => {
                render_state_placeholder(area, frame, "\u{2205}", message, PackedRgba::rgb(140, 140, 140));
            }
            Self::Error { message } => {
                render_state_placeholder(area, frame, "\u{26A0}", message, PackedRgba::rgb(255, 120, 80));
            }
            Self::Ready(widget) => widget.render(area, frame),
        }
    }
}

/// Render a centered placeholder with icon and message for non-data states.
fn render_state_placeholder(
    area: Rect,
    frame: &mut Frame,
    icon: &str,
    message: &str,
    color: PackedRgba,
) {
    if !frame.buffer.degradation.render_content() {
        return;
    }
    let text = format!("{icon} {message}");
    let truncated: String = text.chars().take(area.width as usize).collect();
    // Center vertically.
    let y = area.y + area.height / 2;
    // Center horizontally.
    let text_len = truncated.chars().count() as u16;
    let x = area.x + area.width.saturating_sub(text_len) / 2;
    let line = Line::from_spans([Span::styled(truncated, Style::new().fg(color))]);
    Paragraph::new(line).render(
        Rect { x, y, width: area.width.saturating_sub(x - area.x), height: 1 },
        frame,
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// HeatmapGrid
// ═══════════════════════════════════════════════════════════════════════════════

/// A 2D grid of colored cells representing normalized values (0.0–1.0).
///
/// Each data cell maps to a terminal cell with a background color from a
/// cold-to-hot gradient. Row and column labels are optional.
///
/// # Fallback Behavior
///
/// - At `DegradationLevel::NoStyling` or worse, renders numeric values instead
///   of colored blocks.
/// - At `DegradationLevel::Skeleton` or worse, renders nothing.
/// - When the area is too small for labels + data, labels are dropped first.
#[derive(Debug, Clone)]
pub struct HeatmapGrid<'a> {
    /// 2D data: `rows[r][c]` — each value normalized to 0.0–1.0.
    data: &'a [Vec<f64>],
    /// Optional row labels (left side).
    row_labels: Option<&'a [&'a str]>,
    /// Optional column labels (top).
    col_labels: Option<&'a [&'a str]>,
    /// Block border.
    block: Option<Block<'a>>,
    /// Character used for filled cells (default: `' '` with colored bg).
    fill_char: char,
    /// Whether to show numeric values inside cells when width allows.
    show_values: bool,
    /// Custom gradient function (overrides default heatmap_gradient).
    custom_gradient: Option<fn(f64) -> PackedRgba>,
}

impl<'a> HeatmapGrid<'a> {
    /// Create a new heatmap from 2D data.
    #[must_use]
    pub fn new(data: &'a [Vec<f64>]) -> Self {
        Self {
            data,
            row_labels: None,
            col_labels: None,
            block: None,
            fill_char: ' ',
            show_values: false,
            custom_gradient: None,
        }
    }

    /// Set optional row labels.
    #[must_use]
    pub fn row_labels(mut self, labels: &'a [&'a str]) -> Self {
        self.row_labels = Some(labels);
        self
    }

    /// Set optional column labels.
    #[must_use]
    pub fn col_labels(mut self, labels: &'a [&'a str]) -> Self {
        self.col_labels = Some(labels);
        self
    }

    /// Set a block border.
    #[must_use]
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Use a custom fill character (default: space with colored background).
    #[must_use]
    pub fn fill_char(mut self, ch: char) -> Self {
        self.fill_char = ch;
        self
    }

    /// Show numeric values inside cells when cell width >= 3.
    #[must_use]
    pub fn show_values(mut self, show: bool) -> Self {
        self.show_values = show;
        self
    }

    /// Use a custom gradient function instead of the default heatmap gradient.
    #[must_use]
    pub fn gradient(mut self, f: fn(f64) -> PackedRgba) -> Self {
        self.custom_gradient = Some(f);
        self
    }

    fn resolve_color(&self, value: f64) -> PackedRgba {
        let clamped = if value.is_nan() { 0.0 } else { value.clamp(0.0, 1.0) };
        if let Some(f) = self.custom_gradient {
            f(clamped)
        } else {
            heatmap_gradient(clamped)
        }
    }
}

impl Widget for HeatmapGrid<'_> {
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() || self.data.is_empty() {
            return;
        }

        let deg = frame.buffer.degradation;
        if !deg.render_content() {
            return;
        }

        // Apply block border if set.
        let inner = if let Some(ref block) = self.block {
            let inner = block.inner(area);
            block.clone().render(area, frame);
            inner
        } else {
            area
        };

        if inner.is_empty() {
            return;
        }

        let max_cols = self.data.iter().map(Vec::len).max().unwrap_or(0);
        if max_cols == 0 {
            return;
        }

        // Compute label gutter width.
        let label_width: u16 = self
            .row_labels
            .map(|labels| {
                labels
                    .iter()
                    .map(|l| l.len())
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1) // space after label
            })
            .unwrap_or(0) as u16;

        // Drop labels if they'd consume >40% of width.
        let effective_label_width =
            if label_width > 0 && label_width * 10 > inner.width * 4 { 0 } else { label_width };

        let has_col_header = self.col_labels.is_some() && inner.height > 2;
        let data_y_start = inner.y + u16::from(has_col_header);
        let data_x_start = inner.x + effective_label_width;
        let data_width = inner.width.saturating_sub(effective_label_width);
        let data_height = inner.height.saturating_sub(u16::from(has_col_header));

        if data_width == 0 || data_height == 0 {
            return;
        }

        // Cell width: divide available width evenly among columns.
        #[allow(clippy::cast_possible_truncation)]
        let cell_w = (data_width / max_cols as u16).max(1);

        // Render column headers.
        if has_col_header {
            if let Some(col_labels) = self.col_labels {
                let y = inner.y;
                for (c, label) in col_labels.iter().enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    let x = data_x_start + (c as u16) * cell_w;
                    if x >= inner.right() {
                        break;
                    }
                    let max_w = cell_w.min(inner.right().saturating_sub(x));
                    let truncated: String = label.chars().take(max_w as usize).collect();
                    for (i, ch) in truncated.chars().enumerate() {
                        let cx = x + i as u16;
                        if cx < inner.right() {
                            let mut cell = Cell::from_char(ch);
                            cell.fg = PackedRgba::rgb(180, 180, 180);
                            frame.buffer.set_fast(cx, y, cell);
                        }
                    }
                }
            }
        }

        let no_styling = deg >= ftui::render::budget::DegradationLevel::NoStyling;

        // Render data cells.
        for (r, row_data) in self.data.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let y = data_y_start + r as u16;
            if y >= inner.bottom() {
                break;
            }

            // Row label.
            if effective_label_width > 0 {
                if let Some(labels) = self.row_labels {
                    if let Some(label) = labels.get(r) {
                        let lbl: String = label
                            .chars()
                            .take((effective_label_width.saturating_sub(1)) as usize)
                            .collect();
                        for (i, ch) in lbl.chars().enumerate() {
                            let cx = inner.x + i as u16;
                            if cx < data_x_start {
                                let mut cell = Cell::from_char(ch);
                                cell.fg = PackedRgba::rgb(180, 180, 180);
                                frame.buffer.set_fast(cx, y, cell);
                            }
                        }
                    }
                }
            }

            // Data cells.
            for (c, &value) in row_data.iter().enumerate() {
                #[allow(clippy::cast_possible_truncation)]
                let x = data_x_start + (c as u16) * cell_w;
                if x >= inner.right() {
                    break;
                }

                let color = self.resolve_color(value);
                let actual_w = cell_w.min(inner.right().saturating_sub(x));

                if no_styling {
                    // Fallback: show numeric value.
                    let txt = format!("{:.0}", value * 100.0);
                    for (i, ch) in txt.chars().enumerate().take(actual_w as usize) {
                        frame.buffer.set_fast(x + i as u16, y, Cell::from_char(ch));
                    }
                } else if self.show_values && actual_w >= 3 {
                    // Show value with colored background.
                    let txt = format!("{:>3.0}", value * 100.0);
                    for (i, ch) in txt.chars().enumerate().take(actual_w as usize) {
                        let mut cell = Cell::from_char(ch);
                        cell.bg = color;
                        cell.fg = contrast_text(color);
                        frame.buffer.set_fast(x + i as u16, y, cell);
                    }
                } else {
                    // Colored block.
                    for dx in 0..actual_w {
                        let mut cell = Cell::from_char(self.fill_char);
                        cell.bg = color;
                        frame.buffer.set_fast(x + dx, y, cell);
                    }
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PercentileRibbon
// ═══════════════════════════════════════════════════════════════════════════════

/// A single time-step of percentile data.
#[derive(Debug, Clone, Copy)]
pub struct PercentileSample {
    /// 50th percentile value.
    pub p50: f64,
    /// 95th percentile value.
    pub p95: f64,
    /// 99th percentile value.
    pub p99: f64,
}

/// Renders stacked percentile bands (p50, p95, p99) over a time series.
///
/// The ribbon displays three horizontal bands per column (time step):
/// - **p99 zone** (top, hot color): area between p95 and p99
/// - **p95 zone** (mid, warm color): area between p50 and p95
/// - **p50 zone** (bottom, cool color): area from 0 to p50
///
/// Values are auto-scaled to fit the available height unless explicit bounds
/// are provided.
///
/// # Fallback
///
/// At `Skeleton` or worse, nothing is rendered.
/// At `NoStyling`, uses ASCII density chars instead of colored blocks.
#[derive(Debug, Clone)]
pub struct PercentileRibbon<'a> {
    /// Time-series samples (left = oldest, right = newest).
    samples: &'a [PercentileSample],
    /// Explicit max bound (auto-derived from data if `None`).
    max: Option<f64>,
    /// Block border.
    block: Option<Block<'a>>,
    /// Color for p50 band.
    color_p50: PackedRgba,
    /// Color for p95 band.
    color_p95: PackedRgba,
    /// Color for p99 band.
    color_p99: PackedRgba,
    /// Optional label (e.g., "Latency ms").
    label: Option<&'a str>,
}

impl<'a> PercentileRibbon<'a> {
    /// Create a ribbon from a time series of percentile samples.
    #[must_use]
    pub fn new(samples: &'a [PercentileSample]) -> Self {
        Self {
            samples,
            max: None,
            block: None,
            color_p50: PackedRgba::rgb(80, 180, 80),   // green
            color_p95: PackedRgba::rgb(220, 180, 50),   // gold
            color_p99: PackedRgba::rgb(255, 80, 80),    // red
            label: None,
        }
    }

    /// Set explicit maximum value.
    #[must_use]
    pub fn max(mut self, max: f64) -> Self {
        self.max = Some(max);
        self
    }

    /// Set a block border.
    #[must_use]
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Override the default band colors.
    #[must_use]
    pub fn colors(mut self, p50: PackedRgba, p95: PackedRgba, p99: PackedRgba) -> Self {
        self.color_p50 = p50;
        self.color_p95 = p95;
        self.color_p99 = p99;
        self
    }

    /// Set an optional label rendered at the top-left.
    #[must_use]
    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    fn auto_max(&self) -> f64 {
        self.max.unwrap_or_else(|| {
            self.samples
                .iter()
                .map(|s| s.p99)
                .fold(0.0_f64, f64::max)
                .max(1.0) // avoid zero-range
        })
    }
}

impl Widget for PercentileRibbon<'_> {
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() || self.samples.is_empty() {
            return;
        }

        if !frame.buffer.degradation.render_content() {
            return;
        }

        let inner = if let Some(ref block) = self.block {
            let inner = block.inner(area);
            block.clone().render(area, frame);
            inner
        } else {
            area
        };

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // Optional label on the first row.
        let (data_area, _label_row) = if let Some(lbl) = self.label {
            if inner.height > 2 {
                let label_y = inner.y;
                for (i, ch) in lbl.chars().enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    let x = inner.x + i as u16;
                    if x >= inner.right() {
                        break;
                    }
                    let mut cell = Cell::from_char(ch);
                    cell.fg = PackedRgba::rgb(180, 180, 180);
                    frame.buffer.set_fast(x, label_y, cell);
                }
                let r = Rect {
                    x: inner.x,
                    y: inner.y + 1,
                    width: inner.width,
                    height: inner.height - 1,
                };
                (r, true)
            } else {
                (inner, false)
            }
        } else {
            (inner, false)
        };

        let max_val = self.auto_max();
        let height = data_area.height as f64;
        let no_styling = frame.buffer.degradation
            >= ftui::render::budget::DegradationLevel::NoStyling;

        // Density chars for no-styling fallback (light to heavy).
        const DENSITY: &[char] = &[' ', '\u{2591}', '\u{2592}', '\u{2593}', '\u{2588}'];

        // Render each column (one per sample, right-aligned to show most recent).
        let width = data_area.width as usize;
        let start_idx = self.samples.len().saturating_sub(width);

        for (col_offset, sample) in self.samples[start_idx..].iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let x = data_area.x + col_offset as u16;
            if x >= data_area.right() {
                break;
            }

            // Compute row thresholds (bottom = 0, top = max).
            let p50_rows = ((sample.p50 / max_val) * height).round() as u16;
            let p95_rows = ((sample.p95 / max_val) * height).round() as u16;
            let p99_rows = ((sample.p99 / max_val) * height).round() as u16;

            // Render from bottom to top.
            for row in 0..data_area.height {
                let y = data_area.bottom().saturating_sub(1).saturating_sub(row);
                if y < data_area.y {
                    break;
                }

                let (color, density_idx) = if row < p50_rows {
                    (self.color_p50, 4) // full block
                } else if row < p95_rows {
                    (self.color_p95, 3)
                } else if row < p99_rows {
                    (self.color_p99, 2)
                } else {
                    continue; // empty
                };

                if no_styling {
                    let ch = DENSITY[density_idx.min(DENSITY.len() - 1)];
                    frame.buffer.set_fast(x, y, Cell::from_char(ch));
                } else {
                    let mut cell = Cell::from_char(' ');
                    cell.bg = color;
                    frame.buffer.set_fast(x, y, cell);
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Leaderboard
// ═══════════════════════════════════════════════════════════════════════════════

/// Direction of rank change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankChange {
    /// Moved up in ranking (positive).
    Up(u32),
    /// Moved down in ranking (negative).
    Down(u32),
    /// New entry (not previously ranked).
    New,
    /// No change.
    Steady,
}

/// A single entry in a leaderboard.
#[derive(Debug, Clone)]
pub struct LeaderboardEntry<'a> {
    /// Display name.
    pub name: &'a str,
    /// Primary metric value (used for ranking).
    pub value: f64,
    /// Optional secondary metric (e.g., "42 calls").
    pub secondary: Option<&'a str>,
    /// Rank change indicator.
    pub change: RankChange,
}

/// Ranked list widget with change indicators and delta values.
///
/// Renders a numbered list with:
/// - Rank number (left)
/// - Change indicator arrow (up/down/new/steady)
/// - Name
/// - Value (right-aligned)
/// - Optional secondary metric
///
/// # Fallback
///
/// At `Skeleton`, nothing is rendered.
#[derive(Debug, Clone)]
pub struct Leaderboard<'a> {
    /// Entries (assumed already sorted by rank, index 0 = #1).
    entries: &'a [LeaderboardEntry<'a>],
    /// Block border.
    block: Option<Block<'a>>,
    /// Format string for the value (default shows 1 decimal place).
    value_suffix: Option<&'a str>,
    /// Maximum entries to display (0 = unlimited).
    max_visible: usize,
    /// Color for "up" change indicators.
    color_up: PackedRgba,
    /// Color for "down" change indicators.
    color_down: PackedRgba,
    /// Color for "new" badge.
    color_new: PackedRgba,
    /// Color for the rank number of the #1 entry.
    color_top: PackedRgba,
}

impl<'a> Leaderboard<'a> {
    /// Create a leaderboard from pre-sorted entries.
    #[must_use]
    pub fn new(entries: &'a [LeaderboardEntry<'a>]) -> Self {
        Self {
            entries,
            block: None,
            value_suffix: None,
            max_visible: 0,
            color_up: PackedRgba::rgb(80, 200, 80),    // green
            color_down: PackedRgba::rgb(255, 80, 80),   // red
            color_new: PackedRgba::rgb(80, 180, 255),   // blue
            color_top: PackedRgba::rgb(255, 215, 0),    // gold
        }
    }

    /// Set a block border.
    #[must_use]
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Set a suffix for displayed values (e.g., "ms", "%", "ops/s").
    #[must_use]
    pub fn value_suffix(mut self, suffix: &'a str) -> Self {
        self.value_suffix = Some(suffix);
        self
    }

    /// Limit the number of visible entries.
    #[must_use]
    pub fn max_visible(mut self, n: usize) -> Self {
        self.max_visible = n;
        self
    }

    /// Override change indicator colors.
    #[must_use]
    pub fn colors(mut self, up: PackedRgba, down: PackedRgba, new: PackedRgba) -> Self {
        self.color_up = up;
        self.color_down = down;
        self.color_new = new;
        self
    }
}

impl Widget for Leaderboard<'_> {
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() || self.entries.is_empty() {
            return;
        }

        if !frame.buffer.degradation.render_content() {
            return;
        }

        let inner = if let Some(ref block) = self.block {
            let inner = block.inner(area);
            block.clone().render(area, frame);
            inner
        } else {
            area
        };

        if inner.width < 10 || inner.height == 0 {
            return;
        }

        let max_entries = if self.max_visible > 0 {
            self.max_visible.min(inner.height as usize)
        } else {
            inner.height as usize
        };

        let no_styling = frame.buffer.degradation
            >= ftui::render::budget::DegradationLevel::NoStyling;

        let mut lines: Vec<Line> = Vec::with_capacity(max_entries);

        for (i, entry) in self.entries.iter().take(max_entries).enumerate() {
            let rank = i + 1;
            let rank_str = format!("{rank:>2}.");

            // Change indicator.
            let (indicator, ind_color) = match entry.change {
                RankChange::Up(n) => (format!("\u{25B2}{n}"), self.color_up),
                RankChange::Down(n) => (format!("\u{25BC}{n}"), self.color_down),
                RankChange::New => ("NEW".to_string(), self.color_new),
                RankChange::Steady => ("\u{2500}\u{2500}".to_string(), PackedRgba::rgb(100, 100, 100)),
            };

            // Value formatting.
            let value_str = if let Some(suffix) = self.value_suffix {
                format!("{:.1}{suffix}", entry.value)
            } else {
                format!("{:.1}", entry.value)
            };

            let rank_color = if rank == 1 && !no_styling {
                self.color_top
            } else {
                PackedRgba::rgb(200, 200, 200)
            };

            let mut spans = vec![
                Span::styled(rank_str, Style::new().fg(rank_color)),
                Span::raw(" "),
                Span::styled(
                    indicator,
                    if no_styling { Style::new() } else { Style::new().fg(ind_color) },
                ),
                Span::raw(" "),
                Span::styled(
                    entry.name.to_string(),
                    Style::new().fg(PackedRgba::rgb(240, 240, 240)),
                ),
            ];

            if let Some(secondary) = entry.secondary {
                spans.push(Span::styled(
                    format!(" ({secondary})"),
                    Style::new().fg(PackedRgba::rgb(120, 120, 120)),
                ));
            }

            // Right-align value: pad between name and value.
            let used: usize = spans.iter().map(|s| s.content.len()).sum();
            let value_len = value_str.len();
            let padding = (inner.width as usize).saturating_sub(used + value_len + 1);
            if padding > 0 {
                spans.push(Span::raw(" ".repeat(padding)));
            }
            spans.push(Span::styled(
                value_str,
                Style::new().fg(PackedRgba::rgb(200, 200, 200)),
            ));

            lines.push(Line::from_spans(spans));
        }

        Paragraph::new(Text::from_lines(lines)).render(inner, frame);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// AnomalyCard
// ═══════════════════════════════════════════════════════════════════════════════

/// Compact anomaly alert card widget.
///
/// Renders a single anomaly alert as a small card with:
/// - Severity badge (colored: Critical/High/Medium/Low)
/// - Confidence bar (percentage)
/// - Headline text
/// - Optional rationale (truncated to fit)
///
/// Designed to be composed in a vertical list or grid layout.
///
/// # Fallback
///
/// At `NoStyling`, severity is shown as text prefix without color.
/// At `Skeleton`, nothing is rendered.
#[derive(Debug, Clone)]
pub struct AnomalyCard<'a> {
    /// Severity level.
    severity: AnomalySeverity,
    /// Confidence score (0.0–1.0).
    confidence: f64,
    /// One-line headline.
    headline: &'a str,
    /// Optional rationale text.
    rationale: Option<&'a str>,
    /// Optional list of next steps.
    next_steps: Option<&'a [&'a str]>,
    /// Whether this card is selected/focused.
    selected: bool,
    /// Block border.
    block: Option<Block<'a>>,
}

/// Severity level for anomaly cards (mirrors `kpi::AnomalySeverity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnomalySeverity {
    /// Informational.
    Low,
    /// Warning.
    Medium,
    /// Problem.
    High,
    /// Emergency.
    Critical,
}

impl AnomalySeverity {
    /// Color for the severity badge.
    #[must_use]
    pub fn color(self) -> PackedRgba {
        match self {
            Self::Low => PackedRgba::rgb(100, 180, 100),
            Self::Medium => PackedRgba::rgb(220, 180, 50),
            Self::High => PackedRgba::rgb(255, 120, 50),
            Self::Critical => PackedRgba::rgb(255, 60, 60),
        }
    }

    /// Short label for display.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "LOW",
            Self::Medium => "MED",
            Self::High => "HIGH",
            Self::Critical => "CRIT",
        }
    }
}

impl<'a> AnomalyCard<'a> {
    /// Create a new anomaly card.
    #[must_use]
    pub fn new(severity: AnomalySeverity, confidence: f64, headline: &'a str) -> Self {
        Self {
            severity,
            confidence,
            headline,
            rationale: None,
            next_steps: None,
            selected: false,
            block: None,
        }
    }

    /// Set the rationale text.
    #[must_use]
    pub fn rationale(mut self, text: &'a str) -> Self {
        self.rationale = Some(text);
        self
    }

    /// Set the next steps list.
    #[must_use]
    pub fn next_steps(mut self, steps: &'a [&'a str]) -> Self {
        self.next_steps = Some(steps);
        self
    }

    /// Mark this card as selected/focused (highlight border).
    #[must_use]
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Set a block border.
    #[must_use]
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Height required to fully render this card.
    #[must_use]
    pub fn required_height(&self) -> u16 {
        let mut h: u16 = 1; // headline + badge line
        h += 1; // confidence bar
        if self.rationale.is_some() {
            h += 1;
        }
        if let Some(steps) = self.next_steps {
            h += steps.len().min(3) as u16;
        }
        if self.block.is_some() {
            h += 2; // top + bottom border
        }
        h
    }
}

impl Widget for AnomalyCard<'_> {
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() {
            return;
        }

        if !frame.buffer.degradation.render_content() {
            return;
        }

        let inner = if let Some(ref block) = self.block {
            let mut blk = block.clone();
            if self.selected {
                blk = blk.border_style(Style::new().fg(self.severity.color()));
            }
            let inner = blk.inner(area);
            blk.render(area, frame);
            inner
        } else {
            area
        };

        if inner.width < 8 || inner.height == 0 {
            return;
        }

        let no_styling = frame.buffer.degradation
            >= ftui::render::budget::DegradationLevel::NoStyling;

        let mut y = inner.y;

        // Line 1: [SEVERITY] headline
        {
            let sev_label = self.severity.label();
            let sev_color = self.severity.color();

            let badge = format!("[{sev_label}]");
            let badge_span = if no_styling {
                Span::raw(badge)
            } else {
                Span::styled(badge, Style::new().fg(sev_color))
            };

            let headline_max = (inner.width as usize).saturating_sub(sev_label.len() + 4);
            let truncated_headline: String =
                self.headline.chars().take(headline_max).collect();

            let line = Line::from_spans([
                badge_span,
                Span::raw(" "),
                Span::styled(
                    truncated_headline,
                    Style::new().fg(PackedRgba::rgb(240, 240, 240)),
                ),
            ]);

            Paragraph::new(line).render(
                Rect { x: inner.x, y, width: inner.width, height: 1 },
                frame,
            );
            y += 1;
        }

        if y >= inner.bottom() {
            return;
        }

        // Line 2: confidence bar.
        {
            let conf_pct = (self.confidence * 100.0).round() as u32;
            let bar_width = (inner.width as usize).saturating_sub(10); // "Conf: XX% " prefix
            let filled = ((self.confidence * bar_width as f64).round() as usize).min(bar_width);
            let empty = bar_width.saturating_sub(filled);

            let conf_color = if self.confidence >= 0.8 {
                PackedRgba::rgb(80, 200, 80)
            } else if self.confidence >= 0.5 {
                PackedRgba::rgb(220, 180, 50)
            } else {
                PackedRgba::rgb(200, 100, 100)
            };

            let spans = if no_styling {
                vec![
                    Span::raw(format!("Conf: {conf_pct:>3}% ")),
                    Span::raw("\u{2588}".repeat(filled)),
                    Span::raw("\u{2591}".repeat(empty)),
                ]
            } else {
                vec![
                    Span::styled(
                        format!("Conf: {conf_pct:>3}% "),
                        Style::new().fg(PackedRgba::rgb(160, 160, 160)),
                    ),
                    Span::styled(
                        "\u{2588}".repeat(filled),
                        Style::new().fg(conf_color),
                    ),
                    Span::styled(
                        "\u{2591}".repeat(empty),
                        Style::new().fg(PackedRgba::rgb(60, 60, 60)),
                    ),
                ]
            };

            Paragraph::new(Line::from_spans(spans)).render(
                Rect { x: inner.x, y, width: inner.width, height: 1 },
                frame,
            );
            y += 1;
        }

        if y >= inner.bottom() {
            return;
        }

        // Line 3: rationale (if present).
        if let Some(rationale) = self.rationale {
            let max_chars = inner.width as usize;
            let truncated: String = rationale.chars().take(max_chars).collect();
            let line = Line::styled(truncated, Style::new().fg(PackedRgba::rgb(160, 160, 160)));
            Paragraph::new(line).render(
                Rect { x: inner.x, y, width: inner.width, height: 1 },
                frame,
            );
            y += 1;
        }

        // Remaining lines: next steps (up to 3).
        if let Some(steps) = self.next_steps {
            for step in steps.iter().take(3) {
                if y >= inner.bottom() {
                    break;
                }
                let bullet = format!("\u{2022} {step}");
                let truncated: String = bullet.chars().take(inner.width as usize).collect();
                let line = Line::styled(truncated, Style::new().fg(PackedRgba::rgb(140, 180, 220)));
                Paragraph::new(line).render(
                    Rect { x: inner.x, y, width: inner.width, height: 1 },
                    frame,
                );
                y += 1;
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Choose black or white text for optimal contrast against a background color.
fn contrast_text(bg: PackedRgba) -> PackedRgba {
    // Relative luminance (simplified sRGB).
    let lum = 0.299 * f64::from(bg.r()) + 0.587 * f64::from(bg.g()) + 0.114 * f64::from(bg.b());
    if lum > 128.0 {
        PackedRgba::rgb(0, 0, 0)
    } else {
        PackedRgba::rgb(255, 255, 255)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// BrailleActivity — braille-canvas activity sparkline
// ═══════════════════════════════════════════════════════════════════════════════

/// Braille-resolution activity visualization using 2x4 sub-pixel rendering.
///
/// Renders a time-series as a filled area chart using Unicode braille characters
/// for much higher visual fidelity than character-level sparklines.
///
/// Each terminal cell encodes 2x4 = 8 sub-pixels, yielding 4x vertical
/// resolution compared to normal text.
///
/// # Fallback
///
/// At `NoStyling`, renders a simple ASCII bar chart.
/// At `Skeleton`, nothing is rendered.
#[derive(Debug, Clone)]
pub struct BrailleActivity<'a> {
    /// Time-series values (left = oldest, right = newest).
    values: &'a [f64],
    /// Explicit max bound (auto-derived if `None`).
    max: Option<f64>,
    /// Block border.
    block: Option<Block<'a>>,
    /// Foreground color for the braille dots.
    color: PackedRgba,
    /// Optional label rendered at the top-left.
    label: Option<&'a str>,
}

impl<'a> BrailleActivity<'a> {
    /// Create a new braille activity chart from time-series data.
    #[must_use]
    pub fn new(values: &'a [f64]) -> Self {
        Self {
            values,
            max: None,
            block: None,
            color: PackedRgba::rgb(80, 200, 120),
            label: None,
        }
    }

    /// Set explicit maximum value.
    #[must_use]
    pub fn max(mut self, max: f64) -> Self {
        self.max = Some(max);
        self
    }

    /// Set a block border.
    #[must_use]
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Set the foreground color for braille dots.
    #[must_use]
    pub fn color(mut self, color: PackedRgba) -> Self {
        self.color = color;
        self
    }

    /// Set an optional label.
    #[must_use]
    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    fn auto_max(&self) -> f64 {
        self.max.unwrap_or_else(|| {
            self.values
                .iter()
                .copied()
                .fold(0.0_f64, f64::max)
                .max(1.0)
        })
    }
}

impl Widget for BrailleActivity<'_> {
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() || self.values.is_empty() {
            return;
        }

        if !frame.buffer.degradation.render_content() {
            return;
        }

        let inner = if let Some(ref block) = self.block {
            let inner = block.inner(area);
            block.clone().render(area, frame);
            inner
        } else {
            area
        };

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // Label row.
        let data_area = if let Some(lbl) = self.label {
            if inner.height > 2 {
                for (i, ch) in lbl.chars().enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    let x = inner.x + i as u16;
                    if x >= inner.right() {
                        break;
                    }
                    let mut cell = Cell::from_char(ch);
                    cell.fg = PackedRgba::rgb(180, 180, 180);
                    frame.buffer.set_fast(x, inner.y, cell);
                }
                Rect {
                    x: inner.x,
                    y: inner.y + 1,
                    width: inner.width,
                    height: inner.height - 1,
                }
            } else {
                inner
            }
        } else {
            inner
        };

        let no_styling = frame.buffer.degradation
            >= ftui::render::budget::DegradationLevel::NoStyling;

        if no_styling {
            // Fallback: simple ASCII bar per column.
            let max_val = self.auto_max();
            let width = data_area.width as usize;
            let start_idx = self.values.len().saturating_sub(width);
            for (col, &val) in self.values[start_idx..].iter().enumerate() {
                #[allow(clippy::cast_possible_truncation)]
                let x = data_area.x + col as u16;
                if x >= data_area.right() {
                    break;
                }
                let ratio = (val / max_val).clamp(0.0, 1.0);
                let filled = (ratio * data_area.height as f64).round() as u16;
                for row in 0..filled {
                    let y = data_area.bottom().saturating_sub(1).saturating_sub(row);
                    if y >= data_area.y {
                        frame.buffer.set_fast(x, y, Cell::from_char('\u{2588}'));
                    }
                }
            }
            return;
        }

        // Braille rendering: use Painter for sub-pixel precision.
        let mut painter = Painter::for_area(data_area, Mode::Braille);
        painter.clear();

        let max_val = self.auto_max();
        let sub_w = data_area.width * Mode::Braille.cols_per_cell();
        let sub_h = data_area.height * Mode::Braille.rows_per_cell();

        // Map values to sub-pixel columns, right-aligned.
        let num_samples = self.values.len().min(sub_w as usize);
        let start_idx = self.values.len().saturating_sub(num_samples);

        for (i, &val) in self.values[start_idx..].iter().enumerate() {
            let ratio = (val / max_val).clamp(0.0, 1.0);
            #[allow(clippy::cast_possible_truncation)]
            let filled_h = (ratio * sub_h as f64).round() as i32;

            let x = (sub_w as usize).saturating_sub(num_samples) + i;
            // Fill from bottom up.
            for dy in 0..filled_h {
                let y = sub_h as i32 - 1 - dy;
                painter.point_colored(x as i32, y, self.color);
            }
        }

        CanvasRef::from_painter(&painter).render(data_area, frame);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MetricTile — compact metric display with inline sparkline
// ═══════════════════════════════════════════════════════════════════════════════

/// Compact metric tile showing a label, current value, trend, and inline sparkline.
///
/// Designed for dashboard grids where many metrics need to be visible at once.
/// Layout: `[Label] [Value] [Trend] [Sparkline]`
///
/// # Fallback
///
/// At `NoStyling`, shows text-only without colored sparkline.
/// At `Skeleton`, nothing is rendered.
#[derive(Debug, Clone)]
pub struct MetricTile<'a> {
    /// Metric name.
    label: &'a str,
    /// Current value (formatted string).
    value: &'a str,
    /// Trend direction.
    trend: MetricTrend,
    /// Recent history for inline sparkline (optional).
    sparkline: Option<&'a [f64]>,
    /// Block border.
    block: Option<Block<'a>>,
    /// Color for the value text.
    value_color: PackedRgba,
}

/// Trend direction for a metric tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricTrend {
    /// Value is increasing.
    Up,
    /// Value is decreasing.
    Down,
    /// Value is stable.
    Flat,
}

impl MetricTrend {
    /// Unicode indicator for this trend.
    #[must_use]
    pub fn indicator(self) -> &'static str {
        match self {
            Self::Up => "\u{25B2}",
            Self::Down => "\u{25BC}",
            Self::Flat => "\u{2500}",
        }
    }

    /// Color for this trend indicator.
    #[must_use]
    pub fn color(self) -> PackedRgba {
        match self {
            Self::Up => PackedRgba::rgb(80, 200, 80),
            Self::Down => PackedRgba::rgb(255, 80, 80),
            Self::Flat => PackedRgba::rgb(140, 140, 140),
        }
    }
}

impl<'a> MetricTile<'a> {
    /// Create a new metric tile.
    #[must_use]
    pub fn new(label: &'a str, value: &'a str, trend: MetricTrend) -> Self {
        Self {
            label,
            value,
            trend,
            sparkline: None,
            block: None,
            value_color: PackedRgba::rgb(240, 240, 240),
        }
    }

    /// Set recent history for inline sparkline.
    #[must_use]
    pub fn sparkline(mut self, data: &'a [f64]) -> Self {
        self.sparkline = Some(data);
        self
    }

    /// Set a block border.
    #[must_use]
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Set the value text color.
    #[must_use]
    pub fn value_color(mut self, color: PackedRgba) -> Self {
        self.value_color = color;
        self
    }
}

/// Unicode sparkline characters (8 levels of vertical bar).
const SPARK_CHARS: &[char] = &[
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}',
    '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];

impl Widget for MetricTile<'_> {
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() {
            return;
        }

        if !frame.buffer.degradation.render_content() {
            return;
        }

        let inner = if let Some(ref block) = self.block {
            let inner = block.inner(area);
            block.clone().render(area, frame);
            inner
        } else {
            area
        };

        if inner.width < 8 || inner.height == 0 {
            return;
        }

        let no_styling = frame.buffer.degradation
            >= ftui::render::budget::DegradationLevel::NoStyling;

        // Line 1: label.
        let label_truncated: String = self.label.chars().take(inner.width as usize).collect();
        let label_line = Line::styled(label_truncated, Style::new().fg(PackedRgba::rgb(160, 160, 160)));
        Paragraph::new(label_line).render(
            Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 },
            frame,
        );

        if inner.height < 2 {
            return;
        }

        // Line 2: value + trend.
        let trend_str = self.trend.indicator();
        let trend_color = if no_styling {
            PackedRgba::rgb(200, 200, 200)
        } else {
            self.trend.color()
        };

        let mut spans = vec![
            Span::styled(self.value.to_string(), Style::new().fg(self.value_color)),
            Span::raw(" "),
            Span::styled(trend_str.to_string(), Style::new().fg(trend_color)),
        ];

        // Inline sparkline from recent history.
        if let Some(data) = self.sparkline {
            let used_len: usize = self.value.len() + 1 + trend_str.len();
            let spark_width = (inner.width as usize).saturating_sub(used_len + 2);
            if spark_width > 0 && !data.is_empty() {
                let max = data.iter().copied().fold(0.0_f64, f64::max).max(1.0);
                let start_idx = data.len().saturating_sub(spark_width);
                let spark_str: String = data[start_idx..]
                    .iter()
                    .map(|&v| {
                        let ratio = (v / max).clamp(0.0, 1.0);
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let idx = (ratio * 7.0).round() as usize;
                        SPARK_CHARS[idx.min(7)]
                    })
                    .collect();
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    spark_str,
                    Style::new().fg(PackedRgba::rgb(100, 160, 200)),
                ));
            }
        }

        let value_line = Line::from_spans(spans);
        Paragraph::new(value_line).render(
            Rect { x: inner.x, y: inner.y + 1, width: inner.width, height: 1 },
            frame,
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// ReservationGauge — file reservation pressure visual
// ═══════════════════════════════════════════════════════════════════════════════

/// Horizontal gauge widget showing reservation pressure (utilization vs capacity).
///
/// Renders a colored bar with percentage, label, and optional TTL countdown.
///
/// # Fallback
///
/// At `NoStyling`, shows text-only percentage.
/// At `Skeleton`, nothing is rendered.
#[derive(Debug, Clone)]
pub struct ReservationGauge<'a> {
    /// Metric label (e.g., "File Reservations").
    label: &'a str,
    /// Current count.
    current: u32,
    /// Maximum capacity.
    capacity: u32,
    /// Optional TTL display (e.g., "12m left").
    ttl_display: Option<&'a str>,
    /// Block border.
    block: Option<Block<'a>>,
    /// Warning threshold (fraction, default 0.7).
    warning_threshold: f64,
    /// Critical threshold (fraction, default 0.9).
    critical_threshold: f64,
}

impl<'a> ReservationGauge<'a> {
    /// Create a new reservation gauge.
    #[must_use]
    pub fn new(label: &'a str, current: u32, capacity: u32) -> Self {
        Self {
            label,
            current,
            capacity,
            ttl_display: None,
            block: None,
            warning_threshold: 0.7,
            critical_threshold: 0.9,
        }
    }

    /// Set the TTL display string.
    #[must_use]
    pub fn ttl_display(mut self, ttl: &'a str) -> Self {
        self.ttl_display = Some(ttl);
        self
    }

    /// Set a block border.
    #[must_use]
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Set warning threshold (default 0.7).
    #[must_use]
    pub fn warning_threshold(mut self, t: f64) -> Self {
        self.warning_threshold = t;
        self
    }

    /// Set critical threshold (default 0.9).
    #[must_use]
    pub fn critical_threshold(mut self, t: f64) -> Self {
        self.critical_threshold = t;
        self
    }

    fn ratio(&self) -> f64 {
        if self.capacity == 0 {
            0.0
        } else {
            (self.current as f64 / self.capacity as f64).clamp(0.0, 1.0)
        }
    }

    fn bar_color(&self) -> PackedRgba {
        let ratio = self.ratio();
        if ratio >= self.critical_threshold {
            PackedRgba::rgb(255, 60, 60)
        } else if ratio >= self.warning_threshold {
            PackedRgba::rgb(220, 180, 50)
        } else {
            PackedRgba::rgb(80, 200, 80)
        }
    }
}

impl Widget for ReservationGauge<'_> {
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() {
            return;
        }

        if !frame.buffer.degradation.render_content() {
            return;
        }

        let inner = if let Some(ref block) = self.block {
            let inner = block.inner(area);
            block.clone().render(area, frame);
            inner
        } else {
            area
        };

        if inner.width < 10 || inner.height == 0 {
            return;
        }

        let no_styling = frame.buffer.degradation
            >= ftui::render::budget::DegradationLevel::NoStyling;

        // Line 1: label + count.
        let count_str = format!("{}/{}", self.current, self.capacity);
        let ttl_suffix = self.ttl_display.map_or(String::new(), |t| format!(" ({t})"));
        let header = format!("{} {count_str}{ttl_suffix}", self.label);
        let header_truncated: String = header.chars().take(inner.width as usize).collect();

        let label_line = Line::styled(header_truncated, Style::new().fg(PackedRgba::rgb(200, 200, 200)));
        Paragraph::new(label_line).render(
            Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 },
            frame,
        );

        if inner.height < 2 {
            return;
        }

        // Line 2: gauge bar.
        let bar_width = inner.width as usize;
        let ratio = self.ratio();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let filled = (ratio * bar_width as f64).round() as usize;
        let empty = bar_width.saturating_sub(filled);
        let pct_str = format!("{:.0}%", ratio * 100.0);

        if no_styling {
            let bar = format!(
                "{}{}  {pct_str}",
                "\u{2588}".repeat(filled),
                "\u{2591}".repeat(empty),
            );
            let truncated: String = bar.chars().take(bar_width).collect();
            let line = Line::from_spans([Span::raw(truncated)]);
            Paragraph::new(line).render(
                Rect { x: inner.x, y: inner.y + 1, width: inner.width, height: 1 },
                frame,
            );
        } else {
            let color = self.bar_color();
            let y = inner.y + 1;
            for dx in 0..inner.width {
                let x = inner.x + dx;
                if (dx as usize) < filled {
                    let mut cell = Cell::from_char(' ');
                    cell.bg = color;
                    frame.buffer.set_fast(x, y, cell);
                } else {
                    let mut cell = Cell::from_char(' ');
                    cell.bg = PackedRgba::rgb(40, 40, 40);
                    frame.buffer.set_fast(x, y, cell);
                }
            }
            // Overlay percentage text centered.
            let pct_start = (bar_width.saturating_sub(pct_str.len())) / 2;
            for (i, ch) in pct_str.chars().enumerate() {
                let x = inner.x + (pct_start + i) as u16;
                if x < inner.right() {
                    let existing_bg = frame.buffer.get(x, y).unwrap().bg;
                    let mut cell = Cell::from_char(ch);
                    cell.bg = existing_bg;
                    cell.fg = contrast_text(existing_bg);
                    frame.buffer.set_fast(x, y, cell);
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// AgentHeatmap — agent-to-agent communication frequency grid
// ═══════════════════════════════════════════════════════════════════════════════

/// Heatmap widget specialized for agent-to-agent communication frequency.
///
/// Wraps [`HeatmapGrid`] with agent-specific semantics: row labels are senders,
/// column labels are receivers, cell values represent normalized message counts.
///
/// # Fallback
///
/// Delegates to `HeatmapGrid`'s fallback behavior.
#[derive(Debug, Clone)]
pub struct AgentHeatmap<'a> {
    /// Agent names used for both row and column labels.
    agents: &'a [&'a str],
    /// Communication matrix: `matrix[sender][receiver]` = message count.
    matrix: &'a [Vec<f64>],
    /// Block border.
    block: Option<Block<'a>>,
    /// Whether to show exact values in cells.
    show_values: bool,
}

impl<'a> AgentHeatmap<'a> {
    /// Create a new agent communication heatmap.
    ///
    /// `matrix[i][j]` is the normalized message count from agent `i` to agent `j`.
    #[must_use]
    pub fn new(agents: &'a [&'a str], matrix: &'a [Vec<f64>]) -> Self {
        Self {
            agents,
            matrix,
            block: None,
            show_values: false,
        }
    }

    /// Set a block border.
    #[must_use]
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Show numeric values inside cells.
    #[must_use]
    pub fn show_values(mut self, show: bool) -> Self {
        self.show_values = show;
        self
    }
}

impl Widget for AgentHeatmap<'_> {
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() || self.matrix.is_empty() || self.agents.is_empty() {
            return;
        }

        let mut heatmap = HeatmapGrid::new(self.matrix)
            .row_labels(self.agents)
            .col_labels(self.agents)
            .show_values(self.show_values);

        if let Some(ref block) = self.block {
            heatmap = heatmap.block(block.clone());
        }

        heatmap.render(area, frame);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Accessibility configuration (br-3vwi.6.3)
// ═══════════════════════════════════════════════════════════════════════════════

/// Accessibility configuration for widget rendering.
///
/// Widgets that accept `A11yConfig` adapt their rendering:
/// - **High contrast**: Replace gradient colors with maximum-contrast pairs (black/white/red/green).
/// - **Reduced motion**: Disable sparkline animation, braille sub-pixel rendering falls back to ASCII.
/// - **Focus visible**: Render a visible focus ring (border highlight) when the widget is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct A11yConfig {
    /// Use maximum-contrast colors (WCAG AAA compliance).
    pub high_contrast: bool,
    /// Disable animation and sub-pixel effects.
    pub reduced_motion: bool,
    /// Always show focus indicator (not just on keyboard navigation).
    pub focus_visible: bool,
}

impl Default for A11yConfig {
    fn default() -> Self {
        Self {
            high_contrast: false,
            reduced_motion: false,
            focus_visible: false,
        }
    }
}

impl A11yConfig {
    /// All accessibility features disabled (default rendering).
    #[must_use]
    pub const fn none() -> Self {
        Self {
            high_contrast: false,
            reduced_motion: false,
            focus_visible: false,
        }
    }

    /// All accessibility features enabled.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            high_contrast: true,
            reduced_motion: true,
            focus_visible: true,
        }
    }

    /// Resolve a gradient color to its high-contrast equivalent.
    ///
    /// Maps the continuous heatmap gradient to a small set of distinct,
    /// high-contrast colors that are distinguishable even with color vision
    /// deficiencies.
    #[must_use]
    pub fn resolve_color(&self, value: f64, normal_color: PackedRgba) -> PackedRgba {
        if !self.high_contrast {
            return normal_color;
        }
        // Map to 4-level high-contrast palette.
        let clamped = value.clamp(0.0, 1.0);
        if clamped < 0.25 {
            PackedRgba::rgb(0, 0, 180)   // blue (cold)
        } else if clamped < 0.50 {
            PackedRgba::rgb(0, 180, 0)   // green (warm)
        } else if clamped < 0.75 {
            PackedRgba::rgb(220, 180, 0) // yellow (hot)
        } else {
            PackedRgba::rgb(220, 0, 0)   // red (critical)
        }
    }

    /// Text color for high-contrast mode.
    #[must_use]
    pub fn text_fg(&self) -> PackedRgba {
        if self.high_contrast {
            PackedRgba::rgb(255, 255, 255)
        } else {
            PackedRgba::rgb(240, 240, 240)
        }
    }

    /// Muted/secondary text color for high-contrast mode.
    #[must_use]
    pub fn muted_fg(&self) -> PackedRgba {
        if self.high_contrast {
            PackedRgba::rgb(200, 200, 200)
        } else {
            PackedRgba::rgb(160, 160, 160)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// DrillDown — keyboard navigation into widget data (br-3vwi.6.3)
// ═══════════════════════════════════════════════════════════════════════════════

/// A single drill-down action that a widget exposes.
///
/// Parent screens collect these from focused widgets and display them as
/// numbered action hints (1-9) in the inspector dock. Users press the
/// corresponding number key to trigger navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrillDownAction {
    /// Human-readable label (e.g., "View agent: RedFox").
    pub label: String,
    /// Navigation target for the app router.
    pub target: DrillDownTarget,
}

/// Navigation target for drill-down actions.
///
/// Mirrors `DeepLinkTarget` but is widget-local to avoid coupling widgets
/// to the screen navigation layer. The screen's `update()` method maps
/// these to `MailScreenMsg::DeepLink(...)` as needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrillDownTarget {
    /// Navigate to an agent detail view.
    Agent(String),
    /// Navigate to a tool metrics view.
    Tool(String),
    /// Navigate to a thread view.
    Thread(String),
    /// Navigate to a message view.
    Message(i64),
    /// Navigate to a timestamp in the timeline.
    Timestamp(i64),
    /// Navigate to a project overview.
    Project(String),
    /// Navigate to a file reservation.
    Reservation(String),
}

/// Trait for widgets that support keyboard drill-down navigation.
///
/// Widgets implementing this trait expose a set of actions based on
/// the currently focused/selected item. The parent screen collects these
/// actions and maps number key presses to navigation commands.
///
/// # Design
///
/// Widgets are stateless renderers — they don't track focus internally.
/// The parent screen passes the selected index and receives back a list
/// of actions. This keeps widgets composable and testable.
pub trait DrillDownWidget {
    /// Return drill-down actions for the currently focused item.
    ///
    /// `selected_index` is the row/cell the user has navigated to.
    /// Returns up to 9 actions (one per number key).
    fn drill_down_actions(&self, selected_index: usize) -> Vec<DrillDownAction>;
}

impl DrillDownWidget for Leaderboard<'_> {
    fn drill_down_actions(&self, selected_index: usize) -> Vec<DrillDownAction> {
        if let Some(entry) = self.entries.get(selected_index) {
            vec![DrillDownAction {
                label: format!("View tool: {}", entry.name),
                target: DrillDownTarget::Tool(entry.name.to_string()),
            }]
        } else {
            vec![]
        }
    }
}

impl DrillDownWidget for AgentHeatmap<'_> {
    fn drill_down_actions(&self, selected_index: usize) -> Vec<DrillDownAction> {
        // selected_index maps to a flattened [row * cols + col] index.
        if self.agents.is_empty() {
            return vec![];
        }
        let cols = self.agents.len();
        let row = selected_index / cols;
        let col = selected_index % cols;

        let mut actions = Vec::new();
        if let Some(&sender) = self.agents.get(row) {
            actions.push(DrillDownAction {
                label: format!("View sender: {sender}"),
                target: DrillDownTarget::Agent(sender.to_string()),
            });
        }
        if let Some(&receiver) = self.agents.get(col) {
            if row != col {
                actions.push(DrillDownAction {
                    label: format!("View receiver: {receiver}"),
                    target: DrillDownTarget::Agent(receiver.to_string()),
                });
            }
        }
        actions
    }
}

impl DrillDownWidget for AnomalyCard<'_> {
    fn drill_down_actions(&self, _selected_index: usize) -> Vec<DrillDownAction> {
        // Anomaly cards offer navigation to the timeline at the anomaly time.
        vec![DrillDownAction {
            label: format!("[{}] {}", self.severity.label(), self.headline),
            target: DrillDownTarget::Tool(self.headline.to_string()),
        }]
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Focus ring — visual focus indicator for keyboard navigation (br-3vwi.6.3)
// ═══════════════════════════════════════════════════════════════════════════════

/// Renders a focus ring (highlighted border) around a widget area.
///
/// Used by parent screens to indicate which widget has keyboard focus.
/// The ring uses the `A11yConfig` to determine visibility and contrast.
pub fn render_focus_ring(area: Rect, frame: &mut Frame, a11y: &A11yConfig) {
    if area.is_empty() || area.width < 3 || area.height < 3 {
        return;
    }

    let color = if a11y.high_contrast {
        PackedRgba::rgb(255, 255, 0) // bright yellow for maximum visibility
    } else {
        PackedRgba::rgb(100, 160, 255) // soft blue
    };

    // Top and bottom edges.
    for x in area.x..area.right() {
        let mut top = Cell::from_char('\u{2500}'); // ─
        top.fg = color;
        frame.buffer.set_fast(x, area.y, top);

        let mut bottom = Cell::from_char('\u{2500}');
        bottom.fg = color;
        frame.buffer.set_fast(x, area.bottom().saturating_sub(1), bottom);
    }

    // Left and right edges.
    for y in area.y..area.bottom() {
        let mut left = Cell::from_char('\u{2502}'); // │
        left.fg = color;
        frame.buffer.set_fast(area.x, y, left);

        let mut right = Cell::from_char('\u{2502}');
        right.fg = color;
        frame.buffer.set_fast(area.right().saturating_sub(1), y, right);
    }

    // Corners.
    let corners = [
        (area.x, area.y, '\u{256D}'),                                              // ╭
        (area.right().saturating_sub(1), area.y, '\u{256E}'),                       // ╮
        (area.x, area.bottom().saturating_sub(1), '\u{2570}'),                      // ╰
        (area.right().saturating_sub(1), area.bottom().saturating_sub(1), '\u{256F}'), // ╯
    ];
    for (x, y, ch) in corners {
        let mut cell = Cell::from_char(ch);
        cell.fg = color;
        frame.buffer.set_fast(x, y, cell);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// AnimationBudget — frame cost tracking and guardrails (br-3vwi.6.3)
// ═══════════════════════════════════════════════════════════════════════════════

/// Tracks cumulative render cost within a frame and enforces a budget.
///
/// Parent screens create one `AnimationBudget` per frame and pass it to
/// widgets that have optional expensive effects (braille rendering,
/// sparkline computation, gradient interpolation). When the budget is
/// exhausted, widgets fall back to cheaper rendering paths.
///
/// # Usage
///
/// ```ignore
/// let mut budget = AnimationBudget::new(Duration::from_millis(8));
/// // ... render widgets, each calling budget.spend() ...
/// if budget.exhausted() {
///     // skip remaining expensive effects
/// }
/// ```
#[derive(Debug, Clone)]
pub struct AnimationBudget {
    /// Maximum allowed render cost for this frame.
    limit: std::time::Duration,
    /// Accumulated render cost so far.
    spent: std::time::Duration,
    /// Whether any widget was forced to degrade.
    degraded: bool,
}

impl AnimationBudget {
    /// Create a new budget with the given frame-time limit.
    #[must_use]
    pub fn new(limit: std::time::Duration) -> Self {
        Self {
            limit,
            spent: std::time::Duration::ZERO,
            degraded: false,
        }
    }

    /// Create a budget for a 60fps target (16.6ms per frame).
    #[must_use]
    pub fn for_60fps() -> Self {
        Self::new(std::time::Duration::from_micros(16_600))
    }

    /// Record render cost for a widget.
    pub fn spend(&mut self, cost: std::time::Duration) {
        self.spent += cost;
        if self.spent > self.limit {
            self.degraded = true;
        }
    }

    /// Returns true if the budget has been exceeded.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.spent > self.limit
    }

    /// Returns true if any widget was degraded during this frame.
    #[must_use]
    pub fn was_degraded(&self) -> bool {
        self.degraded
    }

    /// Remaining budget (zero if exhausted).
    #[must_use]
    pub fn remaining(&self) -> std::time::Duration {
        self.limit.saturating_sub(self.spent)
    }

    /// Fraction of budget consumed (0.0–1.0+).
    #[must_use]
    pub fn utilization(&self) -> f64 {
        if self.limit.is_zero() {
            return 1.0;
        }
        self.spent.as_secs_f64() / self.limit.as_secs_f64()
    }

    /// Time a closure and automatically record its cost.
    pub fn timed<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let start = std::time::Instant::now();
        let result = f();
        self.spend(start.elapsed());
        result
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use ftui::GraphemePool;
    use ftui::layout::Rect;

    fn render_widget(widget: &impl Widget, width: u16, height: u16) -> String {
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(width, height, &mut pool);
        let area = Rect::new(0, 0, width, height);
        widget.render(area, &mut frame);

        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                let cell = frame.buffer.get(x, y).unwrap();
                let ch = cell.content.as_char().unwrap_or(' ');
                out.push(ch);
            }
            out.push('\n');
        }
        out
    }

    // ─── HeatmapGrid tests ─────────────────────────────────────────────

    #[test]
    fn heatmap_empty_data() {
        let data: Vec<Vec<f64>> = vec![];
        let widget = HeatmapGrid::new(&data);
        let out = render_widget(&widget, 20, 5);
        // All spaces — nothing rendered.
        assert!(out.chars().filter(|&c| c != ' ' && c != '\n').count() == 0);
    }

    #[test]
    fn heatmap_single_cell() {
        let data = vec![vec![0.5]];
        let widget = HeatmapGrid::new(&data);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(10, 3, &mut pool);
        widget.render(Rect::new(0, 0, 10, 3), &mut frame);
        // The cell at (0,0) should have a colored background.
        let cell = frame.buffer.get(0, 0).unwrap();
        assert_ne!(cell.bg, PackedRgba::TRANSPARENT, "cell should have colored bg");
    }

    #[test]
    fn heatmap_with_labels() {
        let data = vec![vec![0.0, 1.0], vec![0.5, 0.8]];
        let row_labels: &[&str] = &["A", "B"];
        let col_labels: &[&str] = &["X", "Y"];
        let widget = HeatmapGrid::new(&data)
            .row_labels(row_labels)
            .col_labels(col_labels);
        let out = render_widget(&widget, 30, 5);
        // Row labels should appear.
        assert!(out.contains('A'), "should contain row label A");
        assert!(out.contains('B'), "should contain row label B");
    }

    #[test]
    fn heatmap_show_values() {
        let data = vec![vec![0.75]];
        let widget = HeatmapGrid::new(&data).show_values(true);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(20, 3, &mut pool);
        widget.render(Rect::new(0, 0, 20, 3), &mut frame);
        // Should render numeric value.
        let out = render_widget(&widget, 20, 3);
        assert!(out.contains("75"), "should show value 75");
    }

    #[test]
    fn heatmap_custom_gradient() {
        let data = vec![vec![0.5]];
        let widget = HeatmapGrid::new(&data).gradient(|_| PackedRgba::rgb(255, 0, 0));
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(10, 3, &mut pool);
        widget.render(Rect::new(0, 0, 10, 3), &mut frame);
        let cell = frame.buffer.get(0, 0).unwrap();
        assert_eq!(cell.bg, PackedRgba::rgb(255, 0, 0));
    }

    #[test]
    fn heatmap_nan_values() {
        let data = vec![vec![f64::NAN, 0.5]];
        let widget = HeatmapGrid::new(&data);
        // Should not panic.
        let _out = render_widget(&widget, 20, 3);
    }

    #[test]
    fn heatmap_tiny_area() {
        let data = vec![vec![0.5, 0.8], vec![0.3, 0.9]];
        let widget = HeatmapGrid::new(&data);
        // 2x1 area — very cramped but should not panic.
        let _out = render_widget(&widget, 2, 1);
    }

    // ─── PercentileRibbon tests ─────────────────────────────────────────

    #[test]
    fn ribbon_empty_samples() {
        let samples: Vec<PercentileSample> = vec![];
        let widget = PercentileRibbon::new(&samples);
        let out = render_widget(&widget, 20, 10);
        assert!(out.chars().filter(|&c| c != ' ' && c != '\n').count() == 0);
    }

    #[test]
    fn ribbon_single_sample() {
        let samples = vec![PercentileSample { p50: 10.0, p95: 20.0, p99: 30.0 }];
        let widget = PercentileRibbon::new(&samples);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(20, 10, &mut pool);
        widget.render(Rect::new(0, 0, 20, 10), &mut frame);
        // Bottom rows should have colored background (p50 band).
        let bottom_cell = frame.buffer.get(0, 9).unwrap();
        assert_ne!(bottom_cell.bg, PackedRgba::TRANSPARENT);
    }

    #[test]
    fn ribbon_multiple_samples() {
        let samples: Vec<PercentileSample> = (0..30)
            .map(|i| {
                let v = i as f64;
                PercentileSample { p50: v, p95: v * 1.5, p99: v * 2.0 }
            })
            .collect();
        let widget = PercentileRibbon::new(&samples);
        let _out = render_widget(&widget, 40, 15);
    }

    #[test]
    fn ribbon_with_label_and_max() {
        let samples = vec![
            PercentileSample { p50: 5.0, p95: 15.0, p99: 25.0 },
            PercentileSample { p50: 8.0, p95: 18.0, p99: 30.0 },
        ];
        let widget = PercentileRibbon::new(&samples)
            .max(50.0)
            .label("Latency ms");
        let out = render_widget(&widget, 30, 10);
        assert!(out.contains("Latency"), "should show label");
    }

    #[test]
    fn ribbon_minimal_height() {
        let samples = vec![PercentileSample { p50: 10.0, p95: 20.0, p99: 30.0 }];
        let widget = PercentileRibbon::new(&samples);
        // Minimal height — should not panic.
        let _out = render_widget(&widget, 20, 1);
    }

    // ─── Leaderboard tests ──────────────────────────────────────────────

    #[test]
    fn leaderboard_empty() {
        let entries: Vec<LeaderboardEntry<'_>> = vec![];
        let widget = Leaderboard::new(&entries);
        let out = render_widget(&widget, 40, 10);
        assert!(out.chars().filter(|&c| c != ' ' && c != '\n').count() == 0);
    }

    #[test]
    fn leaderboard_basic() {
        let entries = vec![
            LeaderboardEntry { name: "send_message", value: 42.5, secondary: Some("120 calls"), change: RankChange::Up(2) },
            LeaderboardEntry { name: "fetch_inbox", value: 31.2, secondary: None, change: RankChange::Steady },
            LeaderboardEntry { name: "register_agent", value: 15.8, secondary: None, change: RankChange::Down(1) },
        ];
        let widget = Leaderboard::new(&entries).value_suffix("ms");
        let out = render_widget(&widget, 60, 10);
        assert!(out.contains("send_message"), "should show top entry");
        assert!(out.contains("fetch_inbox"), "should show second entry");
        assert!(out.contains("42.5ms"), "should show value with suffix");
    }

    #[test]
    fn leaderboard_new_entry() {
        let entries = vec![
            LeaderboardEntry { name: "newcomer", value: 99.0, secondary: None, change: RankChange::New },
        ];
        let widget = Leaderboard::new(&entries);
        let out = render_widget(&widget, 40, 5);
        assert!(out.contains("NEW"), "should show NEW badge");
    }

    #[test]
    fn leaderboard_max_visible() {
        let entries = vec![
            LeaderboardEntry { name: "a", value: 10.0, secondary: None, change: RankChange::Steady },
            LeaderboardEntry { name: "b", value: 8.0, secondary: None, change: RankChange::Steady },
            LeaderboardEntry { name: "c", value: 6.0, secondary: None, change: RankChange::Steady },
        ];
        let widget = Leaderboard::new(&entries).max_visible(2);
        let out = render_widget(&widget, 40, 10);
        assert!(out.contains('a'));
        assert!(out.contains('b'));
        assert!(!out.contains("c "), "third entry should be hidden");
    }

    #[test]
    fn leaderboard_narrow_area() {
        let entries = vec![
            LeaderboardEntry { name: "test", value: 1.0, secondary: None, change: RankChange::Steady },
        ];
        let widget = Leaderboard::new(&entries);
        // Width < 10 — should render nothing gracefully.
        let out = render_widget(&widget, 8, 5);
        assert!(out.chars().filter(|&c| c != ' ' && c != '\n').count() == 0);
    }

    // ─── AnomalyCard tests ──────────────────────────────────────────────

    #[test]
    fn anomaly_card_basic() {
        let widget = AnomalyCard::new(
            AnomalySeverity::High,
            0.85,
            "Tool call p95 latency exceeded threshold",
        );
        let out = render_widget(&widget, 60, 5);
        assert!(out.contains("[HIGH]"), "should show severity badge");
        assert!(out.contains("Tool call"), "should show headline");
        assert!(out.contains("85%"), "should show confidence");
    }

    #[test]
    fn anomaly_card_with_rationale() {
        let widget = AnomalyCard::new(AnomalySeverity::Critical, 0.95, "Error rate spike")
            .rationale("Error rate increased 5x in the last 60 seconds");
        let out = render_widget(&widget, 60, 5);
        assert!(out.contains("[CRIT]"));
        assert!(out.contains("Error rate"));
    }

    #[test]
    fn anomaly_card_with_steps() {
        let steps: &[&str] = &["Check logs", "Restart service"];
        let widget = AnomalyCard::new(AnomalySeverity::Medium, 0.6, "Utilization high")
            .next_steps(steps);
        let out = render_widget(&widget, 50, 8);
        assert!(out.contains("Check logs"));
        assert!(out.contains("Restart"));
    }

    #[test]
    fn anomaly_card_required_height() {
        let basic = AnomalyCard::new(AnomalySeverity::Low, 0.5, "Test");
        assert_eq!(basic.required_height(), 2); // headline + confidence

        let with_rationale = AnomalyCard::new(AnomalySeverity::Low, 0.5, "Test")
            .rationale("Some rationale");
        assert_eq!(with_rationale.required_height(), 3);

        let steps: &[&str] = &["Step 1", "Step 2"];
        let with_steps = AnomalyCard::new(AnomalySeverity::Low, 0.5, "Test")
            .next_steps(steps);
        assert_eq!(with_steps.required_height(), 4); // headline + confidence + 2 steps
    }

    #[test]
    fn anomaly_card_selected() {
        use ftui::widgets::borders::BorderType;
        let widget = AnomalyCard::new(AnomalySeverity::Critical, 0.9, "Alert!")
            .selected(true)
            .block(Block::new().borders(ftui::widgets::borders::Borders::ALL).border_type(BorderType::Rounded));
        // Should not panic.
        let _out = render_widget(&widget, 40, 6);
    }

    #[test]
    fn anomaly_card_tiny_area() {
        let widget = AnomalyCard::new(AnomalySeverity::Low, 0.5, "Test headline");
        // Very small area — should not panic.
        let _out = render_widget(&widget, 5, 1);
    }

    // ─── Severity tests ─────────────────────────────────────────────────

    #[test]
    fn severity_ordering() {
        assert!(AnomalySeverity::Low < AnomalySeverity::Medium);
        assert!(AnomalySeverity::Medium < AnomalySeverity::High);
        assert!(AnomalySeverity::High < AnomalySeverity::Critical);
    }

    #[test]
    fn severity_labels() {
        assert_eq!(AnomalySeverity::Low.label(), "LOW");
        assert_eq!(AnomalySeverity::Medium.label(), "MED");
        assert_eq!(AnomalySeverity::High.label(), "HIGH");
        assert_eq!(AnomalySeverity::Critical.label(), "CRIT");
    }

    #[test]
    fn severity_colors_distinct() {
        let colors: Vec<PackedRgba> = [
            AnomalySeverity::Low,
            AnomalySeverity::Medium,
            AnomalySeverity::High,
            AnomalySeverity::Critical,
        ]
        .iter()
        .map(|s| s.color())
        .collect();

        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "severity colors should be distinct");
            }
        }
    }

    // ─── Contrast helper tests ──────────────────────────────────────────

    #[test]
    fn contrast_text_light_bg() {
        let result = contrast_text(PackedRgba::rgb(255, 255, 255));
        assert_eq!(result, PackedRgba::rgb(0, 0, 0), "light bg → black text");
    }

    #[test]
    fn contrast_text_dark_bg() {
        let result = contrast_text(PackedRgba::rgb(0, 0, 0));
        assert_eq!(result, PackedRgba::rgb(255, 255, 255), "dark bg → white text");
    }

    // ─── RankChange tests ───────────────────────────────────────────────

    #[test]
    fn rank_change_variants() {
        assert_eq!(RankChange::Up(3), RankChange::Up(3));
        assert_ne!(RankChange::Up(1), RankChange::Down(1));
        assert_eq!(RankChange::Steady, RankChange::Steady);
        assert_eq!(RankChange::New, RankChange::New);
    }

    // ─── WidgetState tests ─────────────────────────────────────────────

    #[test]
    fn widget_state_loading() {
        let state: WidgetState<'_, HeatmapGrid<'_>> =
            WidgetState::Loading { message: "Fetching metrics..." };
        let out = render_widget(&state, 40, 5);
        assert!(out.contains("Fetching"), "loading state should show message");
    }

    #[test]
    fn widget_state_empty() {
        let state: WidgetState<'_, HeatmapGrid<'_>> =
            WidgetState::Empty { message: "No data available" };
        let out = render_widget(&state, 40, 5);
        assert!(out.contains("No data"), "empty state should show message");
    }

    #[test]
    fn widget_state_error() {
        let state: WidgetState<'_, HeatmapGrid<'_>> =
            WidgetState::Error { message: "Connection failed" };
        let out = render_widget(&state, 40, 5);
        assert!(out.contains("Connection"), "error state should show message");
    }

    #[test]
    fn widget_state_ready() {
        let data = vec![vec![0.5]];
        let heatmap = HeatmapGrid::new(&data);
        let state = WidgetState::Ready(heatmap);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(20, 5, &mut pool);
        state.render(Rect::new(0, 0, 20, 5), &mut frame);
        // Ready state should render the inner widget.
        let cell = frame.buffer.get(0, 0).unwrap();
        assert_ne!(cell.bg, PackedRgba::TRANSPARENT);
    }

    // ─── BrailleActivity tests ─────────────────────────────────────────

    #[test]
    fn braille_activity_empty() {
        let values: Vec<f64> = vec![];
        let widget = BrailleActivity::new(&values);
        let out = render_widget(&widget, 40, 10);
        assert!(out.chars().filter(|&c| c != ' ' && c != '\n').count() == 0);
    }

    #[test]
    fn braille_activity_single_value() {
        let values = vec![50.0];
        let widget = BrailleActivity::new(&values).max(100.0);
        // Should not panic.
        let _out = render_widget(&widget, 40, 10);
    }

    #[test]
    fn braille_activity_many_values() {
        let values: Vec<f64> = (0..100).map(|i| (i as f64).sin().abs() * 100.0).collect();
        let widget = BrailleActivity::new(&values)
            .label("Activity")
            .color(PackedRgba::rgb(100, 200, 255));
        let out = render_widget(&widget, 60, 15);
        assert!(out.contains("Activity"), "should show label");
    }

    #[test]
    fn braille_activity_tiny_area() {
        let values = vec![1.0, 2.0, 3.0];
        let widget = BrailleActivity::new(&values);
        // Should not panic even at 1x1.
        let _out = render_widget(&widget, 1, 1);
    }

    // ─── MetricTile tests ──────────────────────────────────────────────

    #[test]
    fn metric_tile_basic() {
        let widget = MetricTile::new("Latency p95", "42ms", MetricTrend::Up);
        let out = render_widget(&widget, 40, 3);
        assert!(out.contains("Latency"), "should show label");
        assert!(out.contains("42ms"), "should show value");
    }

    #[test]
    fn metric_tile_with_sparkline() {
        let history = [10.0, 15.0, 12.0, 18.0, 20.0, 25.0];
        let widget = MetricTile::new("Throughput", "250 ops/s", MetricTrend::Up)
            .sparkline(&history);
        let out = render_widget(&widget, 50, 3);
        assert!(out.contains("Throughput"));
        assert!(out.contains("250"));
    }

    #[test]
    fn metric_tile_tiny_area() {
        let widget = MetricTile::new("X", "1", MetricTrend::Flat);
        // Width < 8 — should not panic.
        let _out = render_widget(&widget, 5, 2);
    }

    #[test]
    fn metric_trend_indicators() {
        assert_eq!(MetricTrend::Up.indicator(), "\u{25B2}");
        assert_eq!(MetricTrend::Down.indicator(), "\u{25BC}");
        assert_eq!(MetricTrend::Flat.indicator(), "\u{2500}");
    }

    #[test]
    fn metric_trend_colors_distinct() {
        let colors = [MetricTrend::Up.color(), MetricTrend::Down.color(), MetricTrend::Flat.color()];
        assert_ne!(colors[0], colors[1]);
        assert_ne!(colors[1], colors[2]);
        assert_ne!(colors[0], colors[2]);
    }

    // ─── ReservationGauge tests ────────────────────────────────────────

    #[test]
    fn reservation_gauge_basic() {
        let widget = ReservationGauge::new("File Reservations", 7, 10);
        let out = render_widget(&widget, 40, 3);
        assert!(out.contains("File Reservations"));
        assert!(out.contains("7/10"));
    }

    #[test]
    fn reservation_gauge_with_ttl() {
        let widget = ReservationGauge::new("Locks", 3, 20).ttl_display("12m left");
        let out = render_widget(&widget, 50, 3);
        assert!(out.contains("12m left"));
    }

    #[test]
    fn reservation_gauge_empty() {
        let widget = ReservationGauge::new("Empty", 0, 10);
        let out = render_widget(&widget, 40, 3);
        assert!(out.contains("0/10"));
    }

    #[test]
    fn reservation_gauge_full() {
        let widget = ReservationGauge::new("Full", 10, 10);
        let out = render_widget(&widget, 40, 3);
        assert!(out.contains("10/10"));
    }

    #[test]
    fn reservation_gauge_zero_capacity() {
        let widget = ReservationGauge::new("Zero", 0, 0);
        // Should not panic.
        let _out = render_widget(&widget, 40, 3);
    }

    #[test]
    fn reservation_gauge_color_thresholds() {
        let low = ReservationGauge::new("L", 3, 10);
        assert_eq!(low.bar_color(), PackedRgba::rgb(80, 200, 80), "below warning = green");

        let warn = ReservationGauge::new("W", 8, 10);
        assert_eq!(warn.bar_color(), PackedRgba::rgb(220, 180, 50), "warning = gold");

        let crit = ReservationGauge::new("C", 10, 10);
        assert_eq!(crit.bar_color(), PackedRgba::rgb(255, 60, 60), "critical = red");
    }

    // ─── AgentHeatmap tests ────────────────────────────────────────────

    #[test]
    fn agent_heatmap_basic() {
        let agents: &[&str] = &["Alpha", "Beta", "Gamma"];
        let matrix = vec![
            vec![0.0, 0.8, 0.3],
            vec![0.5, 0.0, 0.9],
            vec![0.2, 0.4, 0.0],
        ];
        let widget = AgentHeatmap::new(agents, &matrix);
        let out = render_widget(&widget, 40, 8);
        assert!(out.contains("Alpha"), "should show agent name");
    }

    #[test]
    fn agent_heatmap_empty_matrix() {
        let agents: &[&str] = &[];
        let matrix: Vec<Vec<f64>> = vec![];
        let widget = AgentHeatmap::new(agents, &matrix);
        let out = render_widget(&widget, 30, 5);
        assert!(out.chars().filter(|&c| c != ' ' && c != '\n').count() == 0);
    }

    #[test]
    fn agent_heatmap_with_values() {
        let agents: &[&str] = &["A", "B"];
        let matrix = vec![vec![0.0, 0.75], vec![0.5, 0.0]];
        let widget = AgentHeatmap::new(agents, &matrix).show_values(true);
        let out = render_widget(&widget, 30, 5);
        assert!(out.contains("75"), "should show value 75");
    }

    // ─── Render-cost performance baselines ────────────────────────────

    /// Render a widget N times and assert total time is under budget.
    fn render_perf(widget: &impl Widget, w: u16, h: u16, iters: u32, budget_us: u128) {
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(w, h, &mut pool);
            widget.render(Rect::new(0, 0, w, h), &mut frame);
        }
        let elapsed_us = start.elapsed().as_micros();
        let per_iter_us = elapsed_us / u128::from(iters);
        eprintln!(
            "  perf: {iters} renders in {elapsed_us}\u{00B5}s ({per_iter_us}\u{00B5}s/iter, budget {budget_us}\u{00B5}s)"
        );
        assert!(
            per_iter_us <= budget_us,
            "render cost {per_iter_us}\u{00B5}s exceeded budget {budget_us}\u{00B5}s"
        );
    }

    #[test]
    fn perf_heatmap_10x10() {
        let data: Vec<Vec<f64>> = (0..10)
            .map(|r| (0..10).map(|c| ((r * 10 + c) as f64) / 100.0).collect())
            .collect();
        let widget = HeatmapGrid::new(&data).show_values(true);
        render_perf(&widget, 80, 24, 500, 500);
    }

    #[test]
    fn perf_percentile_ribbon_100_samples() {
        let samples: Vec<PercentileSample> = (0..100)
            .map(|i| {
                let v = (i as f64 * 0.1).sin().abs() * 50.0;
                PercentileSample { p50: v, p95: v * 1.5, p99: v * 2.0 }
            })
            .collect();
        let widget = PercentileRibbon::new(&samples).label("Latency ms");
        render_perf(&widget, 120, 30, 500, 500);
    }

    #[test]
    fn perf_leaderboard_20_entries() {
        let entries: Vec<LeaderboardEntry<'_>> = (0..20)
            .map(|i| LeaderboardEntry {
                name: "agent_tool_call",
                value: 100.0 - i as f64 * 4.0,
                secondary: Some("42 calls"),
                change: if i % 3 == 0 { RankChange::Up(1) } else { RankChange::Steady },
            })
            .collect();
        let widget = Leaderboard::new(&entries).value_suffix("ms");
        render_perf(&widget, 60, 24, 500, 500);
    }

    #[test]
    fn perf_anomaly_card() {
        let steps: &[&str] = &["Check logs", "Restart service", "Escalate"];
        let widget = AnomalyCard::new(AnomalySeverity::Critical, 0.92, "Error rate spike detected")
            .rationale("5x increase in error rate over 60s window")
            .next_steps(steps);
        render_perf(&widget, 60, 8, 1000, 200);
    }

    #[test]
    fn perf_braille_activity_200_values() {
        let values: Vec<f64> = (0..200).map(|i| (i as f64 * 0.05).sin().abs() * 100.0).collect();
        let widget = BrailleActivity::new(&values).label("Activity");
        render_perf(&widget, 80, 20, 200, 2000);
    }

    #[test]
    fn perf_metric_tile_with_sparkline() {
        let history: Vec<f64> = (0..50).map(|i| (i as f64 * 0.1).sin().abs() * 100.0).collect();
        let widget = MetricTile::new("Latency p95", "42.3ms", MetricTrend::Up)
            .sparkline(&history);
        render_perf(&widget, 50, 3, 1000, 200);
    }

    #[test]
    fn perf_reservation_gauge() {
        let widget = ReservationGauge::new("File Reservations", 7, 10)
            .ttl_display("12m left");
        render_perf(&widget, 50, 3, 1000, 200);
    }

    #[test]
    fn perf_agent_heatmap_5x5() {
        let agents: &[&str] = &["Alpha", "Beta", "Gamma", "Delta", "Epsilon"];
        let matrix: Vec<Vec<f64>> = (0..5)
            .map(|r| (0..5).map(|c| if r == c { 0.0 } else { (r * 5 + c) as f64 / 25.0 }).collect())
            .collect();
        let widget = AgentHeatmap::new(agents, &matrix).show_values(true);
        render_perf(&widget, 60, 10, 500, 500);
    }

    #[test]
    fn perf_widget_state_variants() {
        let loading: WidgetState<'_, HeatmapGrid<'_>> =
            WidgetState::Loading { message: "Fetching metrics..." };
        render_perf(&loading, 40, 5, 1000, 100);

        let empty: WidgetState<'_, HeatmapGrid<'_>> =
            WidgetState::Empty { message: "No data" };
        render_perf(&empty, 40, 5, 1000, 100);

        let error: WidgetState<'_, HeatmapGrid<'_>> =
            WidgetState::Error { message: "Connection failed" };
        render_perf(&error, 40, 5, 1000, 100);
    }

    // ─── A11yConfig tests ─────────────────────────────────────────────

    #[test]
    fn a11y_default_is_disabled() {
        let cfg = A11yConfig::default();
        assert!(!cfg.high_contrast);
        assert!(!cfg.reduced_motion);
        assert!(!cfg.focus_visible);
    }

    #[test]
    fn a11y_all_enables_everything() {
        let cfg = A11yConfig::all();
        assert!(cfg.high_contrast);
        assert!(cfg.reduced_motion);
        assert!(cfg.focus_visible);
    }

    #[test]
    fn a11y_resolve_color_passthrough() {
        let cfg = A11yConfig::none();
        let color = PackedRgba::rgb(42, 100, 200);
        assert_eq!(cfg.resolve_color(0.5, color), color, "no-a11y should passthrough");
    }

    #[test]
    fn a11y_resolve_color_high_contrast_bands() {
        let cfg = A11yConfig { high_contrast: true, ..A11yConfig::none() };
        let dummy = PackedRgba::rgb(128, 128, 128);

        let cold = cfg.resolve_color(0.1, dummy);
        let warm = cfg.resolve_color(0.3, dummy);
        let hot = cfg.resolve_color(0.6, dummy);
        let critical = cfg.resolve_color(0.9, dummy);

        // All four bands should be distinct.
        let colors = [cold, warm, hot, critical];
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "high-contrast bands {i} and {j} should differ");
            }
        }
    }

    #[test]
    fn a11y_text_colors() {
        let normal = A11yConfig::none();
        let hc = A11yConfig { high_contrast: true, ..A11yConfig::none() };

        // High contrast text should be brighter.
        assert_eq!(hc.text_fg(), PackedRgba::rgb(255, 255, 255));
        assert_eq!(normal.text_fg(), PackedRgba::rgb(240, 240, 240));

        // High contrast muted should be brighter than normal muted.
        assert!(hc.muted_fg().r() > normal.muted_fg().r());
    }

    // ─── DrillDown tests ──────────────────────────────────────────────

    #[test]
    fn leaderboard_drill_down_valid_index() {
        let entries = vec![
            LeaderboardEntry { name: "send_message", value: 42.5, secondary: None, change: RankChange::Steady },
            LeaderboardEntry { name: "fetch_inbox", value: 31.2, secondary: None, change: RankChange::Steady },
        ];
        let widget = Leaderboard::new(&entries);
        let actions = widget.drill_down_actions(0);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].label.contains("send_message"));
        assert_eq!(actions[0].target, DrillDownTarget::Tool("send_message".to_string()));
    }

    #[test]
    fn leaderboard_drill_down_out_of_bounds() {
        let entries = vec![
            LeaderboardEntry { name: "test", value: 1.0, secondary: None, change: RankChange::Steady },
        ];
        let widget = Leaderboard::new(&entries);
        let actions = widget.drill_down_actions(99);
        assert!(actions.is_empty(), "out-of-bounds should return empty");
    }

    #[test]
    fn agent_heatmap_drill_down() {
        let agents: &[&str] = &["Alpha", "Beta", "Gamma"];
        let matrix = vec![
            vec![0.0, 0.8, 0.3],
            vec![0.5, 0.0, 0.9],
            vec![0.2, 0.4, 0.0],
        ];
        let widget = AgentHeatmap::new(agents, &matrix);

        // Cell (1, 2) = Beta→Gamma: should get sender=Beta, receiver=Gamma.
        let actions = widget.drill_down_actions(1 * 3 + 2);
        assert_eq!(actions.len(), 2);
        assert!(actions[0].label.contains("Beta"), "sender should be Beta");
        assert!(actions[1].label.contains("Gamma"), "receiver should be Gamma");

        // Diagonal cell (0, 0) = Alpha→Alpha: only one action (no self-link).
        let actions = widget.drill_down_actions(0);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].label.contains("Alpha"));
    }

    #[test]
    fn agent_heatmap_drill_down_empty() {
        let agents: &[&str] = &[];
        let matrix: Vec<Vec<f64>> = vec![];
        let widget = AgentHeatmap::new(agents, &matrix);
        let actions = widget.drill_down_actions(0);
        assert!(actions.is_empty());
    }

    #[test]
    fn anomaly_card_drill_down() {
        let widget = AnomalyCard::new(
            AnomalySeverity::High,
            0.85,
            "Latency spike",
        );
        let actions = widget.drill_down_actions(0);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].label.contains("[HIGH]"));
        assert!(actions[0].label.contains("Latency spike"));
    }

    // ─── Focus ring tests ──────────────────────────────────────────────

    #[test]
    fn focus_ring_renders_corners() {
        let a11y = A11yConfig::none();
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(10, 5, &mut pool);
        render_focus_ring(Rect::new(0, 0, 10, 5), &mut frame, &a11y);

        // Check corners have round box-drawing chars.
        let tl = frame.buffer.get(0, 0).unwrap();
        assert_eq!(tl.content.as_char().unwrap(), '\u{256D}', "top-left corner");
        let tr = frame.buffer.get(9, 0).unwrap();
        assert_eq!(tr.content.as_char().unwrap(), '\u{256E}', "top-right corner");
    }

    #[test]
    fn focus_ring_high_contrast_uses_yellow() {
        let a11y = A11yConfig { high_contrast: true, ..A11yConfig::none() };
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(10, 5, &mut pool);
        render_focus_ring(Rect::new(0, 0, 10, 5), &mut frame, &a11y);

        let cell = frame.buffer.get(1, 0).unwrap(); // top edge
        assert_eq!(cell.fg, PackedRgba::rgb(255, 255, 0), "high-contrast ring should be yellow");
    }

    #[test]
    fn focus_ring_too_small_is_noop() {
        let a11y = A11yConfig::none();
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(2, 2, &mut pool);
        render_focus_ring(Rect::new(0, 0, 2, 2), &mut frame, &a11y);
        // Area too small (< 3x3) — nothing rendered.
        let cell = frame.buffer.get(0, 0).unwrap();
        assert_ne!(cell.content.as_char().unwrap_or(' '), '\u{256D}');
    }

    // ─── AnimationBudget tests ─────────────────────────────────────────

    #[test]
    fn budget_starts_fresh() {
        let budget = AnimationBudget::for_60fps();
        assert!(!budget.exhausted());
        assert!(!budget.was_degraded());
        assert!(budget.utilization() < 0.01);
    }

    #[test]
    fn budget_tracks_spending() {
        let mut budget = AnimationBudget::new(std::time::Duration::from_millis(10));
        budget.spend(std::time::Duration::from_millis(3));
        assert!(!budget.exhausted());
        assert!((budget.utilization() - 0.3).abs() < 0.01);

        budget.spend(std::time::Duration::from_millis(8));
        assert!(budget.exhausted());
        assert!(budget.was_degraded());
        assert!(budget.remaining().is_zero());
    }

    #[test]
    fn budget_timed_records_cost() {
        let mut budget = AnimationBudget::new(std::time::Duration::from_secs(1));
        let result = budget.timed(|| {
            // A tiny computation.
            42
        });
        assert_eq!(result, 42);
        assert!(budget.utilization() > 0.0);
    }

    #[test]
    fn budget_zero_limit() {
        let budget = AnimationBudget::new(std::time::Duration::ZERO);
        assert_eq!(budget.utilization(), 1.0, "zero limit should show 100% utilization");
    }
}
