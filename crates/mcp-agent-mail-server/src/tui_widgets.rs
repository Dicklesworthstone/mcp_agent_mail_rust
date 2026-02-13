//! Advanced composable widgets for the TUI operations console.
//!
//! Eight reusable widgets designed for signal density and low render overhead:
//!
//! - [`HeatmapGrid`]: 2D colored cell grid with configurable gradient
//! - [`PercentileRibbon`]: p50/p95/p99 latency bands over time
//! - [`Leaderboard`]: Ranked list with change indicators and delta values
//! - [`AnomalyCard`]: Compact anomaly alert card with severity/confidence badges
//! - [`MetricTile`]: Compact metric display with inline sparkline
//! - [`ReservationGauge`]: Reservation pressure bar (ProgressBar-backed)
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
use ftui_extras::charts::heatmap_gradient;
use ftui_widgets::progress::ProgressBar;
use ftui_widgets::sparkline::Sparkline;

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
                render_state_placeholder(
                    area,
                    frame,
                    "\u{23F3}",
                    message,
                    PackedRgba::rgb(120, 160, 220),
                );
            }
            Self::Empty { message } => {
                render_state_placeholder(
                    area,
                    frame,
                    "\u{2205}",
                    message,
                    PackedRgba::rgb(140, 140, 140),
                );
            }
            Self::Error { message } => {
                render_state_placeholder(
                    area,
                    frame,
                    "\u{26A0}",
                    message,
                    PackedRgba::rgb(255, 120, 80),
                );
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
    #[allow(clippy::cast_possible_truncation)]
    let text_len = truncated.chars().count() as u16;
    let x = area.x + area.width.saturating_sub(text_len) / 2;
    let line = Line::from_spans([Span::styled(truncated, Style::new().fg(color))]);
    Paragraph::new(line).render(
        Rect {
            x,
            y,
            width: area.width.saturating_sub(x - area.x),
            height: 1,
        },
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
    /// Custom gradient function (overrides default `heatmap_gradient`).
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
    pub const fn row_labels(mut self, labels: &'a [&'a str]) -> Self {
        self.row_labels = Some(labels);
        self
    }

    /// Set optional column labels.
    #[must_use]
    pub const fn col_labels(mut self, labels: &'a [&'a str]) -> Self {
        self.col_labels = Some(labels);
        self
    }

    /// Set a block border.
    #[must_use]
    pub const fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Use a custom fill character (default: space with colored background).
    #[must_use]
    pub const fn fill_char(mut self, ch: char) -> Self {
        self.fill_char = ch;
        self
    }

    /// Show numeric values inside cells when cell width >= 3.
    #[must_use]
    pub const fn show_values(mut self, show: bool) -> Self {
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
        let clamped = if value.is_nan() {
            0.0
        } else {
            value.clamp(0.0, 1.0)
        };
        self.custom_gradient
            .map_or_else(|| heatmap_gradient(clamped), |f| f(clamped))
    }
}

impl Widget for HeatmapGrid<'_> {
    #[allow(clippy::too_many_lines)]
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() || self.data.is_empty() {
            return;
        }

        let deg = frame.buffer.degradation;
        if !deg.render_content() {
            return;
        }

        // Apply block border if set.
        let inner = self.block.as_ref().map_or(area, |block| {
            let inner = block.inner(area);
            block.clone().render(area, frame);
            inner
        });

        if inner.is_empty() {
            return;
        }

        let max_cols = self.data.iter().map(Vec::len).max().unwrap_or(0);
        if max_cols == 0 {
            return;
        }

        // Compute label gutter width.
        #[allow(clippy::cast_possible_truncation)]
        let label_width: u16 = self.row_labels.map_or(0, |labels| {
            labels
                .iter()
                .map(|l| l.len())
                .max()
                .unwrap_or(0)
                .saturating_add(1) // space after label
        }) as u16;

        // Drop labels if they'd consume >40% of width.
        let effective_label_width = if label_width > 0 && label_width * 10 > inner.width * 4 {
            0
        } else {
            label_width
        };

        let has_col_header = self.col_labels.is_some() && inner.height > 2;
        let grid_top = inner.y + u16::from(has_col_header);
        let grid_left = inner.x + effective_label_width;
        let data_w = inner.width.saturating_sub(effective_label_width);
        let data_h = inner.height.saturating_sub(u16::from(has_col_header));

        if data_w == 0 || data_h == 0 {
            return;
        }

        // Cell width: divide available width evenly among columns.
        #[allow(clippy::cast_possible_truncation)]
        let cell_w = (data_w / max_cols as u16).max(1);

        // Render column headers.
        if has_col_header {
            if let Some(col_labels) = self.col_labels {
                let y = inner.y;
                for (c, label) in col_labels.iter().enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    let x = grid_left + (c as u16) * cell_w;
                    if x >= inner.right() {
                        break;
                    }
                    let max_w = cell_w.min(inner.right().saturating_sub(x));
                    let truncated: String = label.chars().take(max_w as usize).collect();
                    for (i, ch) in truncated.chars().enumerate() {
                        #[allow(clippy::cast_possible_truncation)]
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
            let y = grid_top + r as u16;
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
                            #[allow(clippy::cast_possible_truncation)]
                            let cx = inner.x + i as u16;
                            if cx < grid_left {
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
                let x = grid_left + (c as u16) * cell_w;
                if x >= inner.right() {
                    break;
                }

                let color = self.resolve_color(value);
                let actual_w = cell_w.min(inner.right().saturating_sub(x));

                if no_styling {
                    // Fallback: show numeric value.
                    let txt = format!("{:.0}", value * 100.0);
                    for (i, ch) in txt.chars().enumerate().take(actual_w as usize) {
                        #[allow(clippy::cast_possible_truncation)]
                        frame.buffer.set_fast(x + i as u16, y, Cell::from_char(ch));
                    }
                } else if self.show_values && actual_w >= 3 {
                    // Show value with colored background.
                    let txt = format!("{:>3.0}", value * 100.0);
                    for (i, ch) in txt.chars().enumerate().take(actual_w as usize) {
                        let mut cell = Cell::from_char(ch);
                        cell.bg = color;
                        cell.fg = contrast_text(color);
                        #[allow(clippy::cast_possible_truncation)]
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
/// Other degradation tiers rely on native `Sparkline` behavior.
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
    pub const fn new(samples: &'a [PercentileSample]) -> Self {
        Self {
            samples,
            max: None,
            block: None,
            color_p50: PackedRgba::rgb(80, 180, 80),  // green
            color_p95: PackedRgba::rgb(220, 180, 50), // gold
            color_p99: PackedRgba::rgb(255, 80, 80),  // red
            label: None,
        }
    }

    /// Set explicit maximum value.
    #[must_use]
    pub const fn max(mut self, max: f64) -> Self {
        self.max = Some(max);
        self
    }

    /// Set a block border.
    #[must_use]
    pub const fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Override the default band colors.
    #[must_use]
    pub const fn colors(mut self, p50: PackedRgba, p95: PackedRgba, p99: PackedRgba) -> Self {
        self.color_p50 = p50;
        self.color_p95 = p95;
        self.color_p99 = p99;
        self
    }

    /// Set an optional label rendered at the top-left.
    #[must_use]
    pub const fn label(mut self, label: &'a str) -> Self {
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

        let inner = self.block.as_ref().map_or(area, |block| {
            let inner = block.inner(area);
            block.clone().render(area, frame);
            inner
        });

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // Optional title row.
        let mut data_area = inner;
        if let Some(lbl) = self.label {
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
            if data_area.height > 1 {
                data_area.y = data_area.y.saturating_add(1);
                data_area.height = data_area.height.saturating_sub(1);
            }
        }

        if data_area.width == 0 || data_area.height == 0 {
            return;
        }

        let legend_width: u16 = if data_area.width >= 10 { 3 } else { 0 };
        let spark_x = data_area.x.saturating_add(legend_width);
        let spark_width = data_area.width.saturating_sub(legend_width);
        if spark_width == 0 {
            return;
        }

        let max_val = self.auto_max();
        let trim_to_width = |values: Vec<f64>| -> Vec<f64> {
            let width = spark_width as usize;
            if values.len() <= width {
                values
            } else {
                values[values.len() - width..].to_vec()
            }
        };

        let p50 = trim_to_width(self.samples.iter().map(|s| s.p50).collect());
        let p95 = trim_to_width(self.samples.iter().map(|s| s.p95).collect());
        let p99 = trim_to_width(self.samples.iter().map(|s| s.p99).collect());

        let top_y = data_area.y;
        let bottom_y = data_area.bottom().saturating_sub(1);
        let mid_y = data_area.y.saturating_add(data_area.height / 2);

        let bands: [(&[f64], &str, PackedRgba, u16); 3] = [
            (&p99, "99", self.color_p99, top_y),
            (&p95, "95", self.color_p95, mid_y),
            (&p50, "50", self.color_p50, bottom_y),
        ];

        let mut last_y: Option<u16> = None;
        for (series, legend, color, y) in bands {
            if Some(y) == last_y || y >= data_area.bottom() {
                continue;
            }
            last_y = Some(y);

            if legend_width > 0 {
                for (idx, ch) in legend.chars().enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    let x = data_area.x + idx as u16;
                    if x >= spark_x {
                        break;
                    }
                    let mut cell = Cell::from_char(ch);
                    cell.fg = color;
                    frame.buffer.set_fast(x, y, cell);
                }
            }

            Sparkline::new(series)
                .bounds(0.0, max_val)
                .style(Style::new().fg(color))
                .render(Rect::new(spark_x, y, spark_width, 1), frame);
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
    pub const fn new(entries: &'a [LeaderboardEntry<'a>]) -> Self {
        Self {
            entries,
            block: None,
            value_suffix: None,
            max_visible: 0,
            color_up: PackedRgba::rgb(80, 200, 80),   // green
            color_down: PackedRgba::rgb(255, 80, 80), // red
            color_new: PackedRgba::rgb(80, 180, 255), // blue
            color_top: PackedRgba::rgb(255, 215, 0),  // gold
        }
    }

    /// Set a block border.
    #[must_use]
    pub const fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Set a suffix for displayed values (e.g., "ms", "%", "ops/s").
    #[must_use]
    pub const fn value_suffix(mut self, suffix: &'a str) -> Self {
        self.value_suffix = Some(suffix);
        self
    }

    /// Limit the number of visible entries.
    #[must_use]
    pub const fn max_visible(mut self, n: usize) -> Self {
        self.max_visible = n;
        self
    }

    /// Override change indicator colors.
    #[must_use]
    pub const fn colors(mut self, up: PackedRgba, down: PackedRgba, new: PackedRgba) -> Self {
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

        let inner = self.block.as_ref().map_or(area, |block| {
            let inner = block.inner(area);
            block.clone().render(area, frame);
            inner
        });

        if inner.width < 10 || inner.height == 0 {
            return;
        }

        let max_entries = if self.max_visible > 0 {
            self.max_visible.min(inner.height as usize)
        } else {
            inner.height as usize
        };

        let no_styling =
            frame.buffer.degradation >= ftui::render::budget::DegradationLevel::NoStyling;

        let mut lines: Vec<Line> = Vec::with_capacity(max_entries);

        for (i, entry) in self.entries.iter().take(max_entries).enumerate() {
            let rank = i + 1;
            let rank_str = format!("{rank:>2}.");

            // Change indicator.
            let (indicator, ind_color) = match entry.change {
                RankChange::Up(n) => (format!("\u{25B2}{n}"), self.color_up),
                RankChange::Down(n) => (format!("\u{25BC}{n}"), self.color_down),
                RankChange::New => ("NEW".to_string(), self.color_new),
                RankChange::Steady => (
                    "\u{2500}\u{2500}".to_string(),
                    PackedRgba::rgb(100, 100, 100),
                ),
            };

            // Value formatting.
            let value_str = self.value_suffix.map_or_else(
                || format!("{:.1}", entry.value),
                |suffix| format!("{:.1}{suffix}", entry.value),
            );

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
                    if no_styling {
                        Style::new()
                    } else {
                        Style::new().fg(ind_color)
                    },
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
    pub const fn color(self) -> PackedRgba {
        match self {
            Self::Low => PackedRgba::rgb(100, 180, 100),
            Self::Medium => PackedRgba::rgb(220, 180, 50),
            Self::High => PackedRgba::rgb(255, 120, 50),
            Self::Critical => PackedRgba::rgb(255, 60, 60),
        }
    }

    /// Short label for display.
    #[must_use]
    pub const fn label(self) -> &'static str {
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
    pub const fn new(severity: AnomalySeverity, confidence: f64, headline: &'a str) -> Self {
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
    pub const fn rationale(mut self, text: &'a str) -> Self {
        self.rationale = Some(text);
        self
    }

    /// Set the next steps list.
    #[must_use]
    pub const fn next_steps(mut self, steps: &'a [&'a str]) -> Self {
        self.next_steps = Some(steps);
        self
    }

    /// Mark this card as selected/focused (highlight border).
    #[must_use]
    pub const fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Set a block border.
    #[must_use]
    pub const fn block(mut self, block: Block<'a>) -> Self {
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
            #[allow(clippy::cast_possible_truncation)]
            {
                h += steps.len().min(3) as u16;
            }
        }
        if self.block.is_some() {
            h += 2; // top + bottom border
        }
        h
    }
}

impl Widget for AnomalyCard<'_> {
    #[allow(clippy::too_many_lines)]
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() {
            return;
        }

        if !frame.buffer.degradation.render_content() {
            return;
        }

        let inner = self.block.as_ref().map_or(area, |block| {
            let mut blk = block.clone();
            if self.selected {
                blk = blk.border_style(Style::new().fg(self.severity.color()));
            }
            let inner = blk.inner(area);
            blk.render(area, frame);
            inner
        });

        if inner.width < 8 || inner.height == 0 {
            return;
        }

        let no_styling =
            frame.buffer.degradation >= ftui::render::budget::DegradationLevel::NoStyling;

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
            let truncated_headline: String = self.headline.chars().take(headline_max).collect();

            let line = Line::from_spans([
                badge_span,
                Span::raw(" "),
                Span::styled(
                    truncated_headline,
                    Style::new().fg(PackedRgba::rgb(240, 240, 240)),
                ),
            ]);

            Paragraph::new(line).render(
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
                frame,
            );
            y += 1;
        }

        if y >= inner.bottom() {
            return;
        }

        // Line 2: confidence bar.
        {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let conf_pct = (self.confidence * 100.0).round() as u32;
            let bar_width = (inner.width as usize).saturating_sub(10); // "Conf: XX% " prefix
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
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
                    Span::styled("\u{2588}".repeat(filled), Style::new().fg(conf_color)),
                    Span::styled(
                        "\u{2591}".repeat(empty),
                        Style::new().fg(PackedRgba::rgb(60, 60, 60)),
                    ),
                ]
            };

            Paragraph::new(Line::from_spans(spans)).render(
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
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
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
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
                    Rect {
                        x: inner.x,
                        y,
                        width: inner.width,
                        height: 1,
                    },
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
    let lum = 0.114f64.mul_add(
        f64::from(bg.b()),
        0.299f64.mul_add(f64::from(bg.r()), 0.587 * f64::from(bg.g())),
    );
    if lum > 128.0 {
        PackedRgba::rgb(0, 0, 0)
    } else {
        PackedRgba::rgb(255, 255, 255)
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
    pub const fn indicator(self) -> &'static str {
        match self {
            Self::Up => "\u{25B2}",
            Self::Down => "\u{25BC}",
            Self::Flat => "\u{2500}",
        }
    }

    /// Color for this trend indicator.
    #[must_use]
    pub const fn color(self) -> PackedRgba {
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
    pub const fn new(label: &'a str, value: &'a str, trend: MetricTrend) -> Self {
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
    pub const fn sparkline(mut self, data: &'a [f64]) -> Self {
        self.sparkline = Some(data);
        self
    }

    /// Set a block border.
    #[must_use]
    pub const fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Set the value text color.
    #[must_use]
    pub const fn value_color(mut self, color: PackedRgba) -> Self {
        self.value_color = color;
        self
    }
}

// NOTE: SPARK_CHARS removed in br-2bbt.4.1 — now using ftui_widgets::Sparkline

impl Widget for MetricTile<'_> {
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() {
            return;
        }

        if !frame.buffer.degradation.render_content() {
            return;
        }

        let inner = self.block.as_ref().map_or(area, |block| {
            let inner = block.inner(area);
            block.clone().render(area, frame);
            inner
        });

        if inner.width < 8 || inner.height == 0 {
            return;
        }

        let no_styling =
            frame.buffer.degradation >= ftui::render::budget::DegradationLevel::NoStyling;

        // Line 1: label.
        let label_truncated: String = self.label.chars().take(inner.width as usize).collect();
        let label_line = Line::styled(
            label_truncated,
            Style::new().fg(PackedRgba::rgb(160, 160, 160)),
        );
        Paragraph::new(label_line).render(
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 1,
            },
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

        // Inline sparkline from recent history (br-2bbt.4.1: now using ftui_widgets::Sparkline).
        if let Some(data) = self.sparkline {
            let used_len: usize = self.value.len() + 1 + trend_str.len();
            let spark_width = (inner.width as usize).saturating_sub(used_len + 2);
            if spark_width > 0 && !data.is_empty() {
                // Take last spark_width values for right-aligned display.
                let start_idx = data.len().saturating_sub(spark_width);
                let slice = &data[start_idx..];
                // Use Sparkline widget's render_to_string() for consistent block-char mapping.
                let spark_str = Sparkline::new(slice).min(0.0).render_to_string();
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    spark_str,
                    Style::new().fg(PackedRgba::rgb(100, 160, 200)),
                ));
            }
        }

        let value_line = Line::from_spans(spans);
        Paragraph::new(value_line).render(
            Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: 1,
            },
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
    pub const fn new(label: &'a str, current: u32, capacity: u32) -> Self {
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
    pub const fn ttl_display(mut self, ttl: &'a str) -> Self {
        self.ttl_display = Some(ttl);
        self
    }

    /// Set a block border.
    #[must_use]
    pub const fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Set warning threshold (default 0.7).
    #[must_use]
    pub const fn warning_threshold(mut self, t: f64) -> Self {
        self.warning_threshold = t;
        self
    }

    /// Set critical threshold (default 0.9).
    #[must_use]
    pub const fn critical_threshold(mut self, t: f64) -> Self {
        self.critical_threshold = t;
        self
    }

    fn ratio(&self) -> f64 {
        if self.capacity == 0 {
            0.0
        } else {
            (f64::from(self.current) / f64::from(self.capacity)).clamp(0.0, 1.0)
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

        let inner = self.block.as_ref().map_or(area, |block| {
            let inner = block.inner(area);
            block.clone().render(area, frame);
            inner
        });

        if inner.width < 10 || inner.height == 0 {
            return;
        }

        // Line 1: label + count.
        let count_str = format!("{}/{}", self.current, self.capacity);
        let ttl_suffix = self
            .ttl_display
            .map_or(String::new(), |t| format!(" ({t})"));
        let header = format!("{} {count_str}{ttl_suffix}", self.label);
        let header_truncated: String = header.chars().take(inner.width as usize).collect();

        let label_line = Line::styled(
            header_truncated,
            Style::new().fg(PackedRgba::rgb(200, 200, 200)),
        );
        Paragraph::new(label_line).render(
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 1,
            },
            frame,
        );

        if inner.height < 2 {
            return;
        }

        // Line 2: ProgressBar-backed gauge bar.
        let ratio = self.ratio();
        let pct_str = format!("{:.0}%", ratio * 100.0);
        ProgressBar::new()
            .ratio(ratio)
            .style(
                Style::new()
                    .bg(PackedRgba::rgb(40, 40, 40))
                    .fg(PackedRgba::rgb(220, 220, 220)),
            )
            .gauge_style(Style::new().bg(self.bar_color()))
            .label(&pct_str)
            .render(
                Rect {
                    x: inner.x,
                    y: inner.y + 1,
                    width: inner.width,
                    height: 1,
                },
                frame,
            );
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
    pub const fn new(agents: &'a [&'a str], matrix: &'a [Vec<f64>]) -> Self {
        Self {
            agents,
            matrix,
            block: None,
            show_values: false,
        }
    }

    /// Set a block border.
    #[must_use]
    pub const fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Show numeric values inside cells.
    #[must_use]
    pub const fn show_values(mut self, show: bool) -> Self {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct A11yConfig {
    /// Use maximum-contrast colors (WCAG AAA compliance).
    pub high_contrast: bool,
    /// Disable animation and sub-pixel effects.
    pub reduced_motion: bool,
    /// Always show focus indicator (not just on keyboard navigation).
    pub focus_visible: bool,
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
            PackedRgba::rgb(0, 0, 180) // blue (cold)
        } else if clamped < 0.50 {
            PackedRgba::rgb(0, 180, 0) // green (warm)
        } else if clamped < 0.75 {
            PackedRgba::rgb(220, 180, 0) // yellow (hot)
        } else {
            PackedRgba::rgb(220, 0, 0) // red (critical)
        }
    }

    /// Text color for high-contrast mode.
    #[must_use]
    pub const fn text_fg(&self) -> PackedRgba {
        if self.high_contrast {
            PackedRgba::rgb(255, 255, 255)
        } else {
            PackedRgba::rgb(240, 240, 240)
        }
    }

    /// Muted/secondary text color for high-contrast mode.
    #[must_use]
    pub const fn muted_fg(&self) -> PackedRgba {
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
    /// Human-readable label (e.g., "View agent: `RedFox`").
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
        self.entries
            .get(selected_index)
            .map_or_else(Vec::new, |entry| {
                vec![DrillDownAction {
                    label: format!("View tool: {}", entry.name),
                    target: DrillDownTarget::Tool(entry.name.to_string()),
                }]
            })
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
        frame
            .buffer
            .set_fast(x, area.bottom().saturating_sub(1), bottom);
    }

    // Left and right edges.
    for y in area.y..area.bottom() {
        let mut left = Cell::from_char('\u{2502}'); // │
        left.fg = color;
        frame.buffer.set_fast(area.x, y, left);

        let mut right = Cell::from_char('\u{2502}');
        right.fg = color;
        frame
            .buffer
            .set_fast(area.right().saturating_sub(1), y, right);
    }

    // Corners.
    let corners = [
        (area.x, area.y, '\u{256D}'),                          // ╭
        (area.right().saturating_sub(1), area.y, '\u{256E}'),  // ╮
        (area.x, area.bottom().saturating_sub(1), '\u{2570}'), // ╰
        (
            area.right().saturating_sub(1),
            area.bottom().saturating_sub(1),
            '\u{256F}',
        ), // ╯
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
    pub const fn new(limit: std::time::Duration) -> Self {
        Self {
            limit,
            spent: std::time::Duration::ZERO,
            degraded: false,
        }
    }

    /// Create a budget for a 60fps target (16.6ms per frame).
    #[must_use]
    pub const fn for_60fps() -> Self {
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
    pub const fn was_degraded(&self) -> bool {
        self.degraded
    }

    /// Remaining budget (zero if exhausted).
    #[must_use]
    pub const fn remaining(&self) -> std::time::Duration {
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
// MessageCard — expandable message card for thread view (br-2bbt.19.1)
// ═══════════════════════════════════════════════════════════════════════════════

/// Expansion state for a message card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageCardState {
    /// Collapsed view: sender line + 80-char preview snippet.
    #[default]
    Collapsed,
    /// Expanded view: full header + separator + markdown body + footer hints.
    Expanded,
}

/// Message importance level for badge rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageImportance {
    /// Normal priority — no badge shown.
    #[default]
    Normal,
    /// Low priority.
    Low,
    /// High priority — shows amber badge.
    High,
    /// Urgent — shows red badge.
    Urgent,
}

impl MessageImportance {
    /// Badge label for display (if any).
    #[must_use]
    pub const fn badge_label(self) -> Option<&'static str> {
        match self {
            Self::Normal | Self::Low => None,
            Self::High => Some("HIGH"),
            Self::Urgent => Some("URGENT"),
        }
    }

    /// Badge color.
    #[must_use]
    pub const fn badge_color(self) -> PackedRgba {
        match self {
            Self::Normal | Self::Low => PackedRgba::rgb(140, 140, 140),
            Self::High => PackedRgba::rgb(220, 160, 50), // amber
            Self::Urgent => PackedRgba::rgb(255, 80, 80), // red
        }
    }
}

/// Palette of 8 distinct colors for sender initial badges.
/// Chosen for good contrast on dark backgrounds and color-blindness friendliness.
const SENDER_BADGE_COLORS: [PackedRgba; 8] = [
    PackedRgba::rgb(66, 133, 244), // blue
    PackedRgba::rgb(52, 168, 83),  // green
    PackedRgba::rgb(251, 188, 4),  // gold
    PackedRgba::rgb(234, 67, 53),  // red
    PackedRgba::rgb(103, 58, 183), // purple
    PackedRgba::rgb(0, 172, 193),  // cyan
    PackedRgba::rgb(255, 112, 67), // orange
    PackedRgba::rgb(124, 179, 66), // lime
];

/// Compute a deterministic color index from a sender name.
///
/// Uses a simple hash (djb2 variant) to map names to one of 8 badge colors.
/// The same name always produces the same color.
#[must_use]
pub fn sender_color_hash(name: &str) -> PackedRgba {
    let mut hash: u32 = 5381;
    for byte in name.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(u32::from(byte));
    }
    let idx = (hash % 8) as usize;
    SENDER_BADGE_COLORS[idx]
}

/// Truncate a body string to approximately `max_chars` characters, breaking at word boundary.
///
/// If truncation occurs, appends "…" ellipsis. Respects word boundaries to avoid
/// cutting words in the middle.
#[must_use]
pub fn truncate_at_word_boundary(body: &str, max_chars: usize) -> String {
    if body.chars().count() <= max_chars {
        return body.to_string();
    }

    // Take characters up to max_chars.
    let truncated: String = body.chars().take(max_chars).collect();

    // Find the last space within the truncated portion for word boundary.
    if let Some(last_space) = truncated.rfind(' ') {
        if last_space > max_chars / 2 {
            // Only break at space if it's not too early in the string.
            return format!("{}…", &truncated[..last_space]);
        }
    }

    // No good word boundary found — hard truncate.
    format!("{truncated}…")
}

/// Expandable message card widget for thread conversation view.
///
/// Renders a single message in either collapsed or expanded state.
/// Collapsed shows a 2-line preview; expanded shows the full message body
/// with markdown rendering.
///
/// # Collapsed Layout (2 lines)
///
/// ```text
/// ┌──────────────────────────────────────────────────────────────────────┐
/// │ [A] AlphaDog · 2m ago · HIGH                                         │
/// │ This is a preview of the message body truncated at word boundary…    │
/// └──────────────────────────────────────────────────────────────────────┘
/// ```
///
/// # Expanded Layout (variable height)
///
/// ```text
/// ┌──────────────────────────────────────────────────────────────────────┐
/// │ [A] AlphaDog · 2m ago · HIGH · #1234                                 │
/// ├──────────────────────────────────────────────────────────────────────┤
/// │ Full message body rendered with markdown formatting.                 │
/// │                                                                      │
/// │ - Bullet points                                                      │
/// │ - Code blocks                                                        │
/// ├──────────────────────────────────────────────────────────────────────┤
/// │ [View Full] [Jump to Sender]                                         │
/// └──────────────────────────────────────────────────────────────────────┘
/// ```
#[derive(Debug, Clone)]
pub struct MessageCard<'a> {
    /// Sender name (e.g., "`AlphaDog`").
    sender: &'a str,
    /// Timestamp display string (e.g., "2m ago", "Jan 5").
    timestamp: &'a str,
    /// Message importance level.
    importance: MessageImportance,
    /// Message ID (shown in expanded view).
    message_id: Option<i64>,
    /// Message body (markdown content).
    body: &'a str,
    /// Current expansion state.
    state: MessageCardState,
    /// Whether this card is selected/focused.
    selected: bool,
    /// Optional block border override.
    block: Option<Block<'a>>,
}

impl<'a> MessageCard<'a> {
    /// Create a new message card.
    #[must_use]
    pub const fn new(sender: &'a str, timestamp: &'a str, body: &'a str) -> Self {
        Self {
            sender,
            timestamp,
            importance: MessageImportance::Normal,
            message_id: None,
            body,
            state: MessageCardState::Collapsed,
            selected: false,
            block: None,
        }
    }

    /// Set the message importance level.
    #[must_use]
    pub const fn importance(mut self, importance: MessageImportance) -> Self {
        self.importance = importance;
        self
    }

    /// Set the message ID (shown in expanded view header).
    #[must_use]
    pub const fn message_id(mut self, id: i64) -> Self {
        self.message_id = Some(id);
        self
    }

    /// Set the expansion state.
    #[must_use]
    pub const fn state(mut self, state: MessageCardState) -> Self {
        self.state = state;
        self
    }

    /// Mark this card as selected/focused (highlight border).
    #[must_use]
    pub const fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Set a custom block border.
    #[must_use]
    pub const fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Get the sender's initial (first character, uppercase).
    fn sender_initial(&self) -> char {
        self.sender
            .chars()
            .next()
            .unwrap_or('?')
            .to_ascii_uppercase()
    }

    /// Get the sender badge color.
    fn sender_color(&self) -> PackedRgba {
        sender_color_hash(self.sender)
    }

    /// Height required to render this card in its current state.
    #[must_use]
    pub fn required_height(&self) -> u16 {
        match self.state {
            MessageCardState::Collapsed => {
                // 2 content lines + 2 border lines.
                4
            }
            MessageCardState::Expanded => {
                // Header: 1 line
                // Separator: 1 line
                // Body: estimate lines from body length (rough: 80 chars/line).
                // Footer: 1 line
                // Borders: 2 lines
                let body_chars = self.body.chars().count();
                #[allow(clippy::cast_possible_truncation)]
                let body_lines = ((body_chars / 60).max(1) + 1) as u16;
                2 + 1 + 1 + body_lines + 1 + 2
            }
        }
    }
}

impl Widget for MessageCard<'_> {
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.is_empty() || area.width < 10 {
            return;
        }

        if !frame.buffer.degradation.render_content() {
            return;
        }

        // Determine border color based on selection and importance.
        let border_color = if self.selected {
            PackedRgba::rgb(100, 160, 255) // soft blue for focus
        } else {
            PackedRgba::rgb(60, 60, 70) // dim border
        };

        // Create block with rounded corners.
        let block = self
            .block
            .clone()
            .unwrap_or_else(|| {
                Block::new()
                    .borders(ftui::widgets::borders::Borders::ALL)
                    .border_type(ftui::widgets::borders::BorderType::Rounded)
            })
            .border_style(Style::new().fg(border_color));

        let inner = block.inner(area);
        block.render(area, frame);

        if inner.width < 8 || inner.height == 0 {
            return;
        }

        match self.state {
            MessageCardState::Collapsed => self.render_collapsed(inner, frame),
            MessageCardState::Expanded => self.render_expanded(inner, frame),
        }
    }
}

impl MessageCard<'_> {
    /// Render collapsed state: sender line + preview snippet.
    fn render_collapsed(&self, inner: Rect, frame: &mut Frame) {
        let mut y = inner.y;

        // Line 1: [Initial] Sender · timestamp · importance badge
        {
            let sender_color = self.sender_color();
            let initial = self.sender_initial();

            // Build spans.
            let mut spans = vec![
                // Badge with colored background.
                Span::styled(
                    format!("[{initial}]"),
                    Style::new()
                        .fg(PackedRgba::rgb(255, 255, 255))
                        .bg(sender_color),
                ),
                Span::raw(" "),
                // Sender name (bold via brighter color).
                Span::styled(
                    self.sender.to_string(),
                    Style::new().fg(PackedRgba::rgb(240, 240, 240)),
                ),
                Span::styled(" · ", Style::new().fg(PackedRgba::rgb(100, 100, 100))),
                // Timestamp (dim).
                Span::styled(
                    self.timestamp.to_string(),
                    Style::new().fg(PackedRgba::rgb(140, 140, 140)),
                ),
            ];

            // Importance badge (if high/urgent).
            if let Some(badge) = self.importance.badge_label() {
                spans.push(Span::styled(
                    " · ",
                    Style::new().fg(PackedRgba::rgb(100, 100, 100)),
                ));
                spans.push(Span::styled(
                    badge.to_string(),
                    Style::new().fg(self.importance.badge_color()),
                ));
            }

            let line = Line::from_spans(spans);
            Paragraph::new(line).render(
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
                frame,
            );
            y += 1;
        }

        if y >= inner.bottom() {
            return;
        }

        // Line 2: Preview snippet (80 chars max, truncated at word boundary).
        {
            // Normalize body: collapse whitespace, remove newlines.
            let normalized: String = self
                .body
                .chars()
                .map(|c| if c.is_whitespace() { ' ' } else { c })
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");

            let preview = truncate_at_word_boundary(&normalized, 80);
            let max_display = (inner.width as usize).saturating_sub(1);
            let display: String = preview.chars().take(max_display).collect();

            let line = Line::styled(display, Style::new().fg(PackedRgba::rgb(160, 160, 160)));
            Paragraph::new(line).render(
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
                frame,
            );
        }
    }

    /// Render expanded state: full header + separator + body + footer.
    #[allow(clippy::too_many_lines)]
    fn render_expanded(&self, inner: Rect, frame: &mut Frame) {
        let mut y = inner.y;

        // Header line: [Initial] Sender · timestamp · importance badge · #message_id
        {
            let sender_color = self.sender_color();
            let initial = self.sender_initial();

            let mut spans = vec![
                Span::styled(
                    format!("[{initial}]"),
                    Style::new()
                        .fg(PackedRgba::rgb(255, 255, 255))
                        .bg(sender_color),
                ),
                Span::raw(" "),
                Span::styled(
                    self.sender.to_string(),
                    Style::new().fg(PackedRgba::rgb(240, 240, 240)),
                ),
                Span::styled(" · ", Style::new().fg(PackedRgba::rgb(100, 100, 100))),
                Span::styled(
                    self.timestamp.to_string(),
                    Style::new().fg(PackedRgba::rgb(140, 140, 140)),
                ),
            ];

            if let Some(badge) = self.importance.badge_label() {
                spans.push(Span::styled(
                    " · ",
                    Style::new().fg(PackedRgba::rgb(100, 100, 100)),
                ));
                spans.push(Span::styled(
                    badge.to_string(),
                    Style::new().fg(self.importance.badge_color()),
                ));
            }

            if let Some(id) = self.message_id {
                spans.push(Span::styled(
                    " · ",
                    Style::new().fg(PackedRgba::rgb(100, 100, 100)),
                ));
                spans.push(Span::styled(
                    format!("#{id}"),
                    Style::new().fg(PackedRgba::rgb(100, 100, 100)),
                ));
            }

            let line = Line::from_spans(spans);
            Paragraph::new(line).render(
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
                frame,
            );
            y += 1;
        }

        if y >= inner.bottom() {
            return;
        }

        // Separator line: thin horizontal rule.
        {
            let rule: String = "─".repeat(inner.width as usize);
            let line = Line::styled(rule, Style::new().fg(PackedRgba::rgb(60, 60, 70)));
            Paragraph::new(line).render(
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
                frame,
            );
            y += 1;
        }

        if y >= inner.bottom() {
            return;
        }

        // Body area: render message body.
        // Reserve 1 line for footer separator and 1 for footer hints.
        let footer_height: u16 = 2;
        let body_height = inner
            .bottom()
            .saturating_sub(y)
            .saturating_sub(footer_height);

        if body_height > 0 {
            // Render body as simple wrapped text.
            // TODO: Use MarkdownRenderer when available (ftui_extras::markdown).
            let body_area = Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: body_height,
            };

            // Word-wrap the body manually for now.
            let wrapped = wrap_text(self.body, inner.width as usize);
            let lines: Vec<Line> = wrapped
                .iter()
                .take(body_height as usize)
                .map(|s| Line::styled(s.clone(), Style::new().fg(PackedRgba::rgb(220, 220, 220))))
                .collect();

            Paragraph::new(Text::from_lines(lines)).render(body_area, frame);
            y += body_height;
        }

        if y >= inner.bottom() {
            return;
        }

        // Footer separator.
        {
            let rule: String = "─".repeat(inner.width as usize);
            let line = Line::styled(rule, Style::new().fg(PackedRgba::rgb(60, 60, 70)));
            Paragraph::new(line).render(
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
                frame,
            );
            y += 1;
        }

        if y >= inner.bottom() {
            return;
        }

        // Footer hints.
        {
            let hints = Line::from_spans([
                Span::styled(
                    "[View Full]",
                    Style::new().fg(PackedRgba::rgb(100, 140, 180)),
                ),
                Span::raw("  "),
                Span::styled(
                    "[Jump to Sender]",
                    Style::new().fg(PackedRgba::rgb(100, 140, 180)),
                ),
            ]);
            Paragraph::new(hints).render(
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
                frame,
            );
        }
    }
}

/// Simple word-wrapping for text at a given width.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();

    for line in text.lines() {
        if line.is_empty() {
            if !current_line.is_empty() {
                lines.push(current_line.clone());
                current_line.clear();
            }
            lines.push(String::new());
            continue;
        }

        for word in line.split_whitespace() {
            if current_line.is_empty() {
                current_line = word.to_string();
            } else if current_line.len() + 1 + word.len() <= width {
                current_line.push(' ');
                current_line.push_str(word);
            } else {
                lines.push(current_line.clone());
                current_line = word.to_string();
            }
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    lines
}

impl DrillDownWidget for MessageCard<'_> {
    fn drill_down_actions(&self, _selected_index: usize) -> Vec<DrillDownAction> {
        let mut actions = vec![DrillDownAction {
            label: format!("View sender: {}", self.sender),
            target: DrillDownTarget::Agent(self.sender.to_string()),
        }];

        if let Some(id) = self.message_id {
            actions.push(DrillDownAction {
                label: format!("View message #{id}"),
                target: DrillDownTarget::Message(id),
            });
        }

        actions
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
        assert_ne!(
            cell.bg,
            PackedRgba::TRANSPARENT,
            "cell should have colored bg"
        );
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
        let samples = vec![PercentileSample {
            p50: 10.0,
            p95: 20.0,
            p99: 30.0,
        }];
        let widget = PercentileRibbon::new(&samples);
        let out = render_widget(&widget, 20, 10);
        assert!(
            out.chars().any(|ch| "▁▂▃▄▅▆▇█".contains(ch)),
            "should render native sparkline glyphs"
        );
    }

    #[test]
    fn ribbon_multiple_samples() {
        let samples: Vec<PercentileSample> = (0..30)
            .map(|i| {
                let v = f64::from(i);
                PercentileSample {
                    p50: v,
                    p95: v * 1.5,
                    p99: v * 2.0,
                }
            })
            .collect();
        let widget = PercentileRibbon::new(&samples);
        let _out = render_widget(&widget, 40, 15);
    }

    #[test]
    fn ribbon_with_label_and_max() {
        let samples = vec![
            PercentileSample {
                p50: 5.0,
                p95: 15.0,
                p99: 25.0,
            },
            PercentileSample {
                p50: 8.0,
                p95: 18.0,
                p99: 30.0,
            },
        ];
        let widget = PercentileRibbon::new(&samples)
            .max(50.0)
            .label("Latency ms");
        let out = render_widget(&widget, 30, 10);
        assert!(out.contains("Latency"), "should show label");
    }

    #[test]
    fn ribbon_minimal_height() {
        let samples = vec![PercentileSample {
            p50: 10.0,
            p95: 20.0,
            p99: 30.0,
        }];
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
            LeaderboardEntry {
                name: "send_message",
                value: 42.5,
                secondary: Some("120 calls"),
                change: RankChange::Up(2),
            },
            LeaderboardEntry {
                name: "fetch_inbox",
                value: 31.2,
                secondary: None,
                change: RankChange::Steady,
            },
            LeaderboardEntry {
                name: "register_agent",
                value: 15.8,
                secondary: None,
                change: RankChange::Down(1),
            },
        ];
        let widget = Leaderboard::new(&entries).value_suffix("ms");
        let out = render_widget(&widget, 60, 10);
        assert!(out.contains("send_message"), "should show top entry");
        assert!(out.contains("fetch_inbox"), "should show second entry");
        assert!(out.contains("42.5ms"), "should show value with suffix");
    }

    #[test]
    fn leaderboard_new_entry() {
        let entries = vec![LeaderboardEntry {
            name: "newcomer",
            value: 99.0,
            secondary: None,
            change: RankChange::New,
        }];
        let widget = Leaderboard::new(&entries);
        let out = render_widget(&widget, 40, 5);
        assert!(out.contains("NEW"), "should show NEW badge");
    }

    #[test]
    fn leaderboard_max_visible() {
        let entries = vec![
            LeaderboardEntry {
                name: "a",
                value: 10.0,
                secondary: None,
                change: RankChange::Steady,
            },
            LeaderboardEntry {
                name: "b",
                value: 8.0,
                secondary: None,
                change: RankChange::Steady,
            },
            LeaderboardEntry {
                name: "c",
                value: 6.0,
                secondary: None,
                change: RankChange::Steady,
            },
        ];
        let widget = Leaderboard::new(&entries).max_visible(2);
        let out = render_widget(&widget, 40, 10);
        assert!(out.contains('a'));
        assert!(out.contains('b'));
        assert!(!out.contains("c "), "third entry should be hidden");
    }

    #[test]
    fn leaderboard_narrow_area() {
        let entries = vec![LeaderboardEntry {
            name: "test",
            value: 1.0,
            secondary: None,
            change: RankChange::Steady,
        }];
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
        let widget =
            AnomalyCard::new(AnomalySeverity::Medium, 0.6, "Utilization high").next_steps(steps);
        let out = render_widget(&widget, 50, 8);
        assert!(out.contains("Check logs"));
        assert!(out.contains("Restart"));
    }

    #[test]
    fn anomaly_card_required_height() {
        let basic = AnomalyCard::new(AnomalySeverity::Low, 0.5, "Test");
        assert_eq!(basic.required_height(), 2); // headline + confidence

        let with_rationale =
            AnomalyCard::new(AnomalySeverity::Low, 0.5, "Test").rationale("Some rationale");
        assert_eq!(with_rationale.required_height(), 3);

        let steps: &[&str] = &["Step 1", "Step 2"];
        let with_steps = AnomalyCard::new(AnomalySeverity::Low, 0.5, "Test").next_steps(steps);
        assert_eq!(with_steps.required_height(), 4); // headline + confidence + 2 steps
    }

    #[test]
    fn anomaly_card_selected() {
        use ftui::widgets::borders::BorderType;
        let widget = AnomalyCard::new(AnomalySeverity::Critical, 0.9, "Alert!")
            .selected(true)
            .block(
                Block::new()
                    .borders(ftui::widgets::borders::Borders::ALL)
                    .border_type(BorderType::Rounded),
            );
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
        assert_eq!(
            result,
            PackedRgba::rgb(255, 255, 255),
            "dark bg → white text"
        );
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
        let state: WidgetState<'_, HeatmapGrid<'_>> = WidgetState::Loading {
            message: "Fetching metrics...",
        };
        let out = render_widget(&state, 40, 5);
        assert!(
            out.contains("Fetching"),
            "loading state should show message"
        );
    }

    #[test]
    fn widget_state_empty() {
        let state: WidgetState<'_, HeatmapGrid<'_>> = WidgetState::Empty {
            message: "No data available",
        };
        let out = render_widget(&state, 40, 5);
        assert!(out.contains("No data"), "empty state should show message");
    }

    #[test]
    fn widget_state_error() {
        let state: WidgetState<'_, HeatmapGrid<'_>> = WidgetState::Error {
            message: "Connection failed",
        };
        let out = render_widget(&state, 40, 5);
        assert!(
            out.contains("Connection"),
            "error state should show message"
        );
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
        let widget =
            MetricTile::new("Throughput", "250 ops/s", MetricTrend::Up).sparkline(&history);
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
        let colors = [
            MetricTrend::Up.color(),
            MetricTrend::Down.color(),
            MetricTrend::Flat.color(),
        ];
        assert_ne!(colors[0], colors[1]);
        assert_ne!(colors[1], colors[2]);
        assert_ne!(colors[0], colors[2]);
    }

    /// Test that `MetricTile` sparkline uses `Sparkline` widget correctly (br-2bbt.4.1).
    #[test]
    fn metric_tile_sparkline_uses_sparkline_widget() {
        // Verify that the sparkline renders block characters from ftui_widgets::Sparkline.
        let history = [0.0, 25.0, 50.0, 75.0, 100.0];
        let widget = MetricTile::new("Test", "100", MetricTrend::Up).sparkline(&history);
        let out = render_widget(&widget, 60, 3);
        // Should contain block chars from Sparkline: ▁▂▃▄▅▆▇█
        // At minimum, the output should contain some Unicode block characters.
        let has_block_chars = out
            .chars()
            .any(|c| matches!(c, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'));
        assert!(
            has_block_chars,
            "MetricTile sparkline should render block characters from Sparkline widget"
        );
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
        assert_eq!(
            low.bar_color(),
            PackedRgba::rgb(80, 200, 80),
            "below warning = green"
        );

        let warn = ReservationGauge::new("W", 8, 10);
        assert_eq!(
            warn.bar_color(),
            PackedRgba::rgb(220, 180, 50),
            "warning = gold"
        );

        let crit = ReservationGauge::new("C", 10, 10);
        assert_eq!(
            crit.bar_color(),
            PackedRgba::rgb(255, 60, 60),
            "critical = red"
        );
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
            .map(|r| (0..10).map(|c| f64::from(r * 10 + c) / 100.0).collect())
            .collect();
        let widget = HeatmapGrid::new(&data).show_values(true);
        render_perf(&widget, 80, 24, 500, 500);
    }

    #[test]
    fn perf_percentile_ribbon_100_samples() {
        let samples: Vec<PercentileSample> = (0..100)
            .map(|i| {
                let v = (f64::from(i) * 0.1).sin().abs() * 50.0;
                PercentileSample {
                    p50: v,
                    p95: v * 1.5,
                    p99: v * 2.0,
                }
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
                value: f64::from(i).mul_add(-4.0, 100.0),
                secondary: Some("42 calls"),
                change: if i % 3 == 0 {
                    RankChange::Up(1)
                } else {
                    RankChange::Steady
                },
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
    fn perf_metric_tile_with_sparkline() {
        let history: Vec<f64> = (0..50)
            .map(|i| (f64::from(i) * 0.1).sin().abs() * 100.0)
            .collect();
        let widget = MetricTile::new("Latency p95", "42.3ms", MetricTrend::Up).sparkline(&history);
        render_perf(&widget, 50, 3, 1000, 200);
    }

    #[test]
    fn perf_reservation_gauge() {
        let widget = ReservationGauge::new("File Reservations", 7, 10).ttl_display("12m left");
        render_perf(&widget, 50, 3, 1000, 200);
    }

    #[test]
    fn perf_agent_heatmap_5x5() {
        let agents: &[&str] = &["Alpha", "Beta", "Gamma", "Delta", "Epsilon"];
        let matrix: Vec<Vec<f64>> = (0..5)
            .map(|r| {
                (0..5)
                    .map(|c| {
                        if r == c {
                            0.0
                        } else {
                            f64::from(r * 5 + c) / 25.0
                        }
                    })
                    .collect()
            })
            .collect();
        let widget = AgentHeatmap::new(agents, &matrix).show_values(true);
        render_perf(&widget, 60, 10, 500, 500);
    }

    #[test]
    fn perf_widget_state_variants() {
        let loading: WidgetState<'_, HeatmapGrid<'_>> = WidgetState::Loading {
            message: "Fetching metrics...",
        };
        render_perf(&loading, 40, 5, 1000, 100);

        let empty: WidgetState<'_, HeatmapGrid<'_>> = WidgetState::Empty { message: "No data" };
        render_perf(&empty, 40, 5, 1000, 100);

        let error: WidgetState<'_, HeatmapGrid<'_>> = WidgetState::Error {
            message: "Connection failed",
        };
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
        assert_eq!(
            cfg.resolve_color(0.5, color),
            color,
            "no-a11y should passthrough"
        );
    }

    #[test]
    fn a11y_resolve_color_high_contrast_bands() {
        let cfg = A11yConfig {
            high_contrast: true,
            ..A11yConfig::none()
        };
        let dummy = PackedRgba::rgb(128, 128, 128);

        let cold = cfg.resolve_color(0.1, dummy);
        let warm = cfg.resolve_color(0.3, dummy);
        let hot = cfg.resolve_color(0.6, dummy);
        let critical = cfg.resolve_color(0.9, dummy);

        // All four bands should be distinct.
        let colors = [cold, warm, hot, critical];
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "high-contrast bands {i} and {j} should differ"
                );
            }
        }
    }

    #[test]
    fn a11y_text_colors() {
        let normal = A11yConfig::none();
        let hc = A11yConfig {
            high_contrast: true,
            ..A11yConfig::none()
        };

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
            LeaderboardEntry {
                name: "send_message",
                value: 42.5,
                secondary: None,
                change: RankChange::Steady,
            },
            LeaderboardEntry {
                name: "fetch_inbox",
                value: 31.2,
                secondary: None,
                change: RankChange::Steady,
            },
        ];
        let widget = Leaderboard::new(&entries);
        let actions = widget.drill_down_actions(0);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].label.contains("send_message"));
        assert_eq!(
            actions[0].target,
            DrillDownTarget::Tool("send_message".to_string())
        );
    }

    #[test]
    fn leaderboard_drill_down_out_of_bounds() {
        let entries = vec![LeaderboardEntry {
            name: "test",
            value: 1.0,
            secondary: None,
            change: RankChange::Steady,
        }];
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
        let actions = widget.drill_down_actions(5);
        assert_eq!(actions.len(), 2);
        assert!(actions[0].label.contains("Beta"), "sender should be Beta");
        assert!(
            actions[1].label.contains("Gamma"),
            "receiver should be Gamma"
        );

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
        let widget = AnomalyCard::new(AnomalySeverity::High, 0.85, "Latency spike");
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
        assert_eq!(
            tr.content.as_char().unwrap(),
            '\u{256E}',
            "top-right corner"
        );
    }

    #[test]
    fn focus_ring_high_contrast_uses_yellow() {
        let a11y = A11yConfig {
            high_contrast: true,
            ..A11yConfig::none()
        };
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(10, 5, &mut pool);
        render_focus_ring(Rect::new(0, 0, 10, 5), &mut frame, &a11y);

        let cell = frame.buffer.get(1, 0).unwrap(); // top edge
        assert_eq!(
            cell.fg,
            PackedRgba::rgb(255, 255, 0),
            "high-contrast ring should be yellow"
        );
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
        assert!(
            (budget.utilization() - 1.0).abs() < f64::EPSILON,
            "zero limit should show 100% utilization"
        );
    }

    // ─── MessageCard tests (br-2bbt.19.1) ────────────────────────────────

    #[test]
    fn message_card_collapsed_truncates_at_word_boundary() {
        // Body longer than 80 chars should truncate at word boundary.
        let long_body = "This is a very long message that should be truncated at a word boundary when rendered in collapsed mode so it fits nicely on the screen.";
        let truncated = truncate_at_word_boundary(long_body, 80);

        assert!(
            truncated.len() <= 81,
            "truncated length {} should be <= 81 (80 + ellipsis)",
            truncated.len()
        );
        assert!(truncated.ends_with('…'), "should end with ellipsis");
        assert!(
            !truncated.ends_with(" …"),
            "should not have space before ellipsis"
        );
    }

    #[test]
    fn message_card_truncate_short_body_unchanged() {
        let short = "Hello world";
        let result = truncate_at_word_boundary(short, 80);
        assert_eq!(result, short, "short body should not be truncated");
    }

    #[test]
    fn message_card_truncate_exact_length() {
        let exact = "a".repeat(80);
        let result = truncate_at_word_boundary(&exact, 80);
        assert_eq!(result, exact, "exact length should not be truncated");
    }

    #[test]
    fn message_card_truncate_no_spaces() {
        let no_spaces = "a".repeat(100);
        let result = truncate_at_word_boundary(&no_spaces, 80);
        assert_eq!(
            result.chars().count(),
            81,
            "no-space body hard truncates at 80 + ellipsis"
        );
        assert!(result.ends_with('…'));
    }

    #[test]
    fn sender_color_hash_deterministic() {
        // Same name should always produce same color.
        let color1 = sender_color_hash("AlphaDog");
        let color2 = sender_color_hash("AlphaDog");
        assert_eq!(color1, color2, "same name should produce same color");

        // Different names should produce potentially different colors.
        let color_other = sender_color_hash("BetaCat");
        // Note: different names may or may not produce different colors due to hash collisions,
        // but the hash should be deterministic.
        let color_other2 = sender_color_hash("BetaCat");
        assert_eq!(
            color_other, color_other2,
            "same name should always produce same color"
        );
    }

    #[test]
    fn sender_color_hash_produces_distinct_colors() {
        // 8 different names should map to potentially different colors.
        let names = [
            "Alpha", "Beta", "Gamma", "Delta", "Epsilon", "Zeta", "Eta", "Theta",
        ];

        let colors: Vec<PackedRgba> = names.iter().map(|n| sender_color_hash(n)).collect();

        // Count distinct colors.
        let mut unique = colors.to_vec();
        unique.sort_by_key(|c| (c.r(), c.g(), c.b()));
        unique.dedup();

        // We expect at least 4 distinct colors from 8 names (due to hash collisions).
        assert!(
            unique.len() >= 4,
            "should have at least 4 distinct colors, got {}",
            unique.len()
        );
    }

    #[test]
    fn sender_color_hash_all_8_palette_colors_reachable() {
        // Verify that all 8 palette colors are reachable by some name.
        let mut found_colors = std::collections::HashSet::new();

        // Try many names to find all palette entries.
        for i in 0..1000 {
            let name = format!("agent_{i}");
            found_colors.insert(sender_color_hash(&name));

            if found_colors.len() == 8 {
                break;
            }
        }

        assert_eq!(
            found_colors.len(),
            8,
            "all 8 palette colors should be reachable"
        );
    }

    #[test]
    fn message_card_collapsed_basic() {
        let widget = MessageCard::new("AlphaDog", "2m ago", "Hello world, this is a test message.")
            .importance(MessageImportance::Normal);
        let out = render_widget(&widget, 60, 6);
        assert!(out.contains('A'), "should show sender initial");
        assert!(out.contains("AlphaDog"), "should show sender name");
        assert!(out.contains("2m ago"), "should show timestamp");
        assert!(out.contains("Hello"), "should show preview");
    }

    #[test]
    fn message_card_collapsed_with_importance() {
        let widget = MessageCard::new("BetaCat", "5m ago", "Urgent message here")
            .importance(MessageImportance::Urgent);
        let out = render_widget(&widget, 60, 6);
        assert!(out.contains("URGENT"), "should show urgent badge");
    }

    #[test]
    fn message_card_expanded_basic() {
        let widget = MessageCard::new(
            "GammaDog",
            "10m ago",
            "Full message body content.\n\nWith multiple paragraphs.",
        )
        .state(MessageCardState::Expanded)
        .message_id(1234);
        let out = render_widget(&widget, 60, 12);
        assert!(out.contains('G'), "should show sender initial");
        assert!(out.contains("GammaDog"), "should show sender name");
        assert!(out.contains("#1234"), "should show message ID");
        assert!(out.contains("View Full"), "should show footer hints");
    }

    #[test]
    fn message_card_expanded_with_importance() {
        let widget = MessageCard::new("DeltaFox", "1h ago", "High priority content")
            .importance(MessageImportance::High)
            .state(MessageCardState::Expanded);
        let out = render_widget(&widget, 60, 10);
        assert!(out.contains("HIGH"), "should show high priority badge");
    }

    #[test]
    fn message_card_required_height_collapsed() {
        let widget = MessageCard::new("Test", "now", "Body").state(MessageCardState::Collapsed);
        assert_eq!(
            widget.required_height(),
            4,
            "collapsed = 2 content + 2 border"
        );
    }

    #[test]
    fn message_card_required_height_expanded() {
        let widget =
            MessageCard::new("Test", "now", "Short body").state(MessageCardState::Expanded);
        // Expanded: header(1) + sep(1) + body(1-2) + footer(1) + sep(1) + border(2)
        let h = widget.required_height();
        assert!(h >= 7, "expanded should be at least 7 lines, got {h}");
    }

    #[test]
    fn message_card_selected_state() {
        let widget = MessageCard::new("Sender", "now", "Content").selected(true);
        // Should not panic.
        let _out = render_widget(&widget, 60, 6);
    }

    #[test]
    fn message_card_tiny_area() {
        let widget = MessageCard::new("S", "now", "Body");
        // Should not panic on tiny area.
        let _out = render_widget(&widget, 5, 2);
    }

    #[test]
    fn message_card_drill_down_actions() {
        let widget = MessageCard::new("AlphaDog", "now", "Content").message_id(42);
        let actions = widget.drill_down_actions(0);
        assert_eq!(actions.len(), 2);
        assert!(actions[0].label.contains("AlphaDog"));
        assert_eq!(
            actions[0].target,
            DrillDownTarget::Agent("AlphaDog".to_string())
        );
        assert!(actions[1].label.contains("#42"));
        assert_eq!(actions[1].target, DrillDownTarget::Message(42));
    }

    #[test]
    fn message_card_drill_down_no_id() {
        let widget = MessageCard::new("BetaCat", "now", "Content");
        let actions = widget.drill_down_actions(0);
        assert_eq!(actions.len(), 1, "no message_id = only sender action");
    }

    #[test]
    fn message_importance_badges() {
        assert!(MessageImportance::Normal.badge_label().is_none());
        assert!(MessageImportance::Low.badge_label().is_none());
        assert_eq!(MessageImportance::High.badge_label(), Some("HIGH"));
        assert_eq!(MessageImportance::Urgent.badge_label(), Some("URGENT"));
    }

    #[test]
    fn message_importance_colors_distinct() {
        let high = MessageImportance::High.badge_color();
        let urgent = MessageImportance::Urgent.badge_color();
        assert_ne!(high, urgent, "high and urgent should have different colors");
    }

    #[test]
    fn wrap_text_basic() {
        let text = "Hello world this is a test";
        let wrapped = wrap_text(text, 12);
        assert!(!wrapped.is_empty());
        for line in &wrapped {
            assert!(line.len() <= 12, "line should fit width");
        }
    }

    #[test]
    fn wrap_text_empty() {
        let wrapped = wrap_text("", 80);
        assert!(wrapped.is_empty());
    }

    #[test]
    fn wrap_text_zero_width() {
        let wrapped = wrap_text("Hello", 0);
        assert!(wrapped.is_empty());
    }

    #[test]
    fn wrap_text_preserves_paragraphs() {
        let text = "First paragraph.\n\nSecond paragraph.";
        let wrapped = wrap_text(text, 80);
        // Should have blank line between paragraphs.
        assert!(
            wrapped.iter().any(String::is_empty),
            "should preserve blank lines"
        );
    }

    // ─── MessageCard snapshot tests ──────────────────────────────────────

    #[test]
    fn snapshot_message_card_collapsed() {
        let widget = MessageCard::new(
            "AlphaDog",
            "2m ago",
            "This is a preview of the message that should be shown in collapsed mode.",
        )
        .importance(MessageImportance::Normal);
        let out = render_widget(&widget, 70, 6);

        // Verify key elements are present.
        assert!(out.contains("[A]"), "should show sender badge");
        assert!(out.contains("AlphaDog"), "should show sender name");
        assert!(out.contains("2m ago"), "should show timestamp");
        assert!(out.contains("preview"), "should show body preview");
    }

    #[test]
    fn snapshot_message_card_expanded() {
        let widget = MessageCard::new(
            "BetaCat",
            "5m ago",
            "# Heading\n\nThis is the full message body.\n\n- Item 1\n- Item 2",
        )
        .importance(MessageImportance::High)
        .message_id(1234)
        .state(MessageCardState::Expanded);
        let out = render_widget(&widget, 70, 14);

        assert!(out.contains("[B]"), "should show sender badge");
        assert!(out.contains("BetaCat"), "should show sender name");
        assert!(out.contains("HIGH"), "should show importance");
        assert!(out.contains("#1234"), "should show message ID");
        assert!(out.contains("Heading"), "should show body content");
        assert!(out.contains("[View Full]"), "should show footer");
    }

    #[test]
    fn snapshot_message_cards_stacked() {
        // Render 3 cards: 2 collapsed, 1 expanded.
        let card1 = MessageCard::new("AlphaDog", "1m ago", "First message preview here")
            .state(MessageCardState::Collapsed);
        let card2 = MessageCard::new(
            "BetaCat",
            "3m ago",
            "Full expanded message content\n\nWith details.",
        )
        .importance(MessageImportance::High)
        .message_id(100)
        .state(MessageCardState::Expanded);
        let card3 = MessageCard::new("GammaDog", "10m ago", "Third message preview")
            .state(MessageCardState::Collapsed);

        // Render each card individually (stacking simulation).
        let out1 = render_widget(&card1, 70, 6);
        let out2 = render_widget(&card2, 70, 12);
        let out3 = render_widget(&card3, 70, 6);

        assert!(out1.contains("AlphaDog"));
        assert!(out2.contains("BetaCat"));
        assert!(
            out2.contains("[View Full]"),
            "expanded card should have footer"
        );
        assert!(out3.contains("GammaDog"));
    }

    #[test]
    fn perf_message_card_collapsed() {
        let widget = MessageCard::new(
            "PerformanceTest",
            "now",
            "This is a performance test message with some content to render.",
        )
        .importance(MessageImportance::Normal);
        render_perf(&widget, 80, 6, 500, 300);
    }

    #[test]
    fn perf_message_card_expanded() {
        let widget = MessageCard::new(
            "PerformanceTest",
            "now",
            "# Performance Test\n\nThis is a longer message body.\n\n- Item 1\n- Item 2\n- Item 3\n\nWith multiple paragraphs of content.",
        )
        .importance(MessageImportance::Urgent)
        .message_id(9999)
        .state(MessageCardState::Expanded);
        render_perf(&widget, 80, 20, 500, 500);
    }
}
