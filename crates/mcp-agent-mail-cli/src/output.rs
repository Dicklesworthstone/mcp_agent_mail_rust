//! CLI output utilities: tables, TTY detection, JSON mode.
//!
//! Provides structured output that automatically adapts:
//! - **JSON mode**: Machine-readable JSON via `--json` flag
//! - **TTY mode**: Styled table output with headers and borders
//! - **Pipe mode**: Clean plain-text tables (no color, no decoration)

#![forbid(unsafe_code)]

use serde::Serialize;
use std::io::IsTerminal;

/// Detect whether stdout is a TTY.
#[must_use]
pub fn is_tty() -> bool {
    std::io::stdout().is_terminal()
}

// ── Simple table renderer ────────────────────────────────────────────────

/// A simple CLI table that auto-sizes columns and renders to text.
///
/// Usage:
/// ```ignore
/// let mut table = CliTable::new(vec!["ID", "NAME", "STATUS"]);
/// table.add_row(vec!["1", "Alice", "active"]);
/// table.add_row(vec!["2", "Bob", "inactive"]);
/// table.render();
/// ```
pub struct CliTable {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    /// Minimum column widths (0 = auto).
    min_widths: Vec<usize>,
}

impl CliTable {
    /// Create a new table with the given column headers.
    pub fn new(headers: Vec<&str>) -> Self {
        let min_widths = vec![0; headers.len()];
        Self {
            headers: headers.into_iter().map(String::from).collect(),
            rows: Vec::new(),
            min_widths,
        }
    }

    /// Add a row of string values.
    pub fn add_row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    /// Set minimum widths for columns.
    pub fn set_min_widths(&mut self, widths: Vec<usize>) {
        self.min_widths = widths;
    }

    /// Compute column widths based on headers and data.
    fn column_widths(&self) -> Vec<usize> {
        let ncols = self.headers.len();
        let mut widths: Vec<usize> = self
            .headers
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let min = self.min_widths.get(i).copied().unwrap_or(0);
                h.len().max(min)
            })
            .collect();

        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < ncols {
                    widths[i] = widths[i].max(cell.len());
                }
            }
        }
        widths
    }

    /// Render the table to stdout.
    pub fn render(&self) {
        let text = self.render_to_string(is_tty());
        for line in text.lines() {
            ftui_runtime::ftui_println!("{line}");
        }
    }

    /// Render the table to a `String`, with TTY-awareness controlled by the
    /// caller. This is the testable core of [`render`].
    pub fn render_to_string(&self, tty: bool) -> String {
        if self.rows.is_empty() {
            return String::new();
        }
        let widths = self.column_widths();
        let mut out = String::new();

        // Header
        let header_line = self.format_row(&self.headers, &widths);
        if tty {
            out.push_str(&format!("\x1b[1m{header_line}\x1b[0m\n"));
        } else {
            out.push_str(&header_line);
            out.push('\n');
        }

        // Separator on TTY
        if tty {
            let sep: String = widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("──");
            out.push_str(&sep);
            out.push('\n');
        }

        // Data rows
        for row in &self.rows {
            let line = self.format_row(row, &widths);
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    fn format_row(&self, cells: &[String], widths: &[usize]) -> String {
        let ncols = widths.len();
        let mut parts = Vec::with_capacity(ncols);
        for (i, width) in widths.iter().enumerate() {
            let cell = cells.get(i).map(String::as_str).unwrap_or("");
            if i == ncols - 1 {
                // Last column: no padding
                parts.push(cell.to_string());
            } else {
                parts.push(format!("{:<width$}", cell, width = *width));
            }
        }
        parts.join("  ")
    }
}

// ── JSON or table output ─────────────────────────────────────────────────

/// Output data as JSON (pretty-printed) or as a table.
///
/// When `json_mode` is true, serializes `data` to JSON.
/// When false, uses the provided render closure for human output.
pub fn json_or_table<T: Serialize, F>(json_mode: bool, data: &T, render: F)
where
    F: FnOnce(),
{
    if json_mode {
        ftui_runtime::ftui_println!(
            "{}",
            serde_json::to_string_pretty(data).unwrap_or_else(|_| "[]".to_string())
        );
    } else {
        render();
    }
}

/// Output an "empty" message or empty JSON array.
pub fn empty_result(json_mode: bool, message: &str) {
    if json_mode {
        ftui_runtime::ftui_println!("[]");
    } else {
        ftui_runtime::ftui_println!("{message}");
    }
}

// ── Status line helpers ──────────────────────────────────────────────────

/// Print a success message with optional checkmark on TTY.
pub fn success(msg: &str) {
    if is_tty() {
        ftui_runtime::ftui_println!("\x1b[32m✓\x1b[0m {msg}");
    } else {
        ftui_runtime::ftui_println!("{msg}");
    }
}

/// Print a warning message.
pub fn warn(msg: &str) {
    if is_tty() {
        ftui_runtime::ftui_eprintln!("\x1b[33m!\x1b[0m {msg}");
    } else {
        ftui_runtime::ftui_eprintln!("{msg}");
    }
}

/// Print an error message.
pub fn error(msg: &str) {
    if is_tty() {
        ftui_runtime::ftui_eprintln!("\x1b[31merror:\x1b[0m {msg}");
    } else {
        ftui_runtime::ftui_eprintln!("error: {msg}");
    }
}

/// Print a section header (bold on TTY).
pub fn section(title: &str) {
    if is_tty() {
        ftui_runtime::ftui_println!("\x1b[1m{title}\x1b[0m");
    } else {
        ftui_runtime::ftui_println!("{title}");
    }
}

// ── Key/value output ─────────────────────────────────────────────────────

/// Print a key-value pair with aligned values.
pub fn kv(key: &str, value: &str) {
    ftui_runtime::ftui_println!("  {key:<20} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_column_widths_from_headers() {
        let table = CliTable::new(vec!["ID", "NAME", "LONG_HEADER"]);
        let widths = table.column_widths();
        assert_eq!(widths, vec![2, 4, 11]);
    }

    #[test]
    fn table_column_widths_expand_for_data() {
        let mut table = CliTable::new(vec!["ID", "NAME"]);
        table.add_row(vec!["1".into(), "Alice".into()]);
        table.add_row(vec!["200".into(), "Bob".into()]);
        let widths = table.column_widths();
        assert_eq!(widths, vec![3, 5]);
    }

    #[test]
    fn table_column_widths_respect_minimums() {
        let mut table = CliTable::new(vec!["X"]);
        table.set_min_widths(vec![10]);
        let widths = table.column_widths();
        assert_eq!(widths, vec![10]);
    }

    #[test]
    fn format_row_pads_correctly() {
        let table = CliTable::new(vec!["A", "B", "C"]);
        let widths = vec![5, 8, 3];
        let row = vec!["hi".into(), "world".into(), "end".into()];
        let line = table.format_row(&row, &widths);
        assert_eq!(line, "hi     world     end");
    }

    #[test]
    fn format_row_last_column_no_padding() {
        let table = CliTable::new(vec!["A", "B"]);
        let widths = vec![10, 10];
        let row = vec!["left".into(), "right".into()];
        let line = table.format_row(&row, &widths);
        // Last column should NOT be padded to width
        assert_eq!(line, "left        right");
    }

    #[test]
    fn is_tty_returns_bool() {
        // In test harness, stdout is not a TTY
        let result = is_tty();
        assert!(!result, "test harness stdout should not be a TTY");
    }

    // ── CLI UX parity tests (br-2ei.5.5) ────────────────────────────────────

    #[test]
    fn table_empty_rows_does_not_render() {
        let table = CliTable::new(vec!["ID", "NAME"]);
        // render() with no rows should be a no-op (no panic)
        // We can't capture stdout easily, but verify it doesn't panic.
        table.render();
    }

    #[test]
    fn table_single_row_widths() {
        let mut table = CliTable::new(vec!["ID", "NAME"]);
        table.add_row(vec!["42".into(), "test-agent".into()]);
        let widths = table.column_widths();
        assert_eq!(widths, vec![2, 10]);
    }

    #[test]
    fn table_many_columns_formatting() {
        let table = CliTable::new(vec!["A", "B", "C", "D"]);
        let widths = vec![3, 5, 4, 6];
        let row = vec!["1".into(), "hello".into(), "ok".into(), "done".into()];
        let line = table.format_row(&row, &widths);
        assert_eq!(line, "1    hello  ok    done");
    }

    #[test]
    fn table_missing_cells_handled() {
        let table = CliTable::new(vec!["A", "B", "C"]);
        let widths = vec![5, 5, 5];
        // Fewer cells than columns
        let row = vec!["x".into()];
        let line = table.format_row(&row, &widths);
        // Missing cells should render as empty
        assert!(line.starts_with("x"));
    }

    #[test]
    fn table_representative_projects_data() {
        let mut table = CliTable::new(vec!["ID", "SLUG", "HUMAN_KEY"]);
        table.add_row(vec![
            "1".into(),
            "backend-api".into(),
            "/home/user/projects/backend".into(),
        ]);
        table.add_row(vec![
            "2".into(),
            "frontend".into(),
            "/home/user/projects/frontend".into(),
        ]);
        let widths = table.column_widths();
        assert_eq!(widths[0], 2); // "ID" header
        assert_eq!(widths[1], 11); // "backend-api" is longest
        assert!(widths[2] >= 27); // human_key path
    }

    #[test]
    fn table_representative_acks_data() {
        let mut table = CliTable::new(vec!["ID", "FROM", "SUBJECT", "IMPORTANCE"]);
        table.add_row(vec![
            "101".into(),
            "GreenCastle".into(),
            "Review needed: auth module".into(),
            "high".into(),
        ]);
        table.add_row(vec![
            "102".into(),
            "BlueLake".into(),
            "Deploy request".into(),
            "urgent".into(),
        ]);
        let widths = table.column_widths();
        assert_eq!(widths[0], 3); // "101"
        assert_eq!(widths[1], 11); // "GreenCastle"
        assert_eq!(widths[2], 26); // "Review needed: auth module"
        assert_eq!(widths[3], 10); // "IMPORTANCE" header
    }

    #[test]
    fn table_representative_reservations_data() {
        let mut table = CliTable::new(vec!["ID", "PATTERN", "AGENT", "EXPIRES", "REASON"]);
        table.add_row(vec![
            "5".into(),
            "src/auth/**/*.ts".into(),
            "RedBear".into(),
            "2026-02-06T18:00:00".into(),
            "bd-123".into(),
        ]);
        let widths = table.column_widths();
        assert!(widths[1] >= 16); // pattern
        assert!(widths[3] >= 19); // ISO timestamp
    }

    // StdioCapture is process-global; serialise tests that install it.
    static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_capture<F: FnOnce()>(body: F) -> String {
        let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let capture = ftui_runtime::StdioCapture::install().unwrap();
        body();
        let out = capture.drain_to_string();
        drop(capture);
        out
    }

    #[test]
    fn json_or_table_json_mode_serializes() {
        let data = vec!["a", "b", "c"];
        let output = with_capture(|| {
            json_or_table(true, &data, || {
                panic!("render should not be called in JSON mode");
            });
        });
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 3);
    }

    #[test]
    fn json_or_table_human_mode_calls_render() {
        let data = vec!["a"];
        let mut called = false;
        json_or_table(false, &data, || {
            called = true;
        });
        assert!(called, "render closure should be called in human mode");
    }

    #[test]
    fn empty_result_json_mode_outputs_empty_array() {
        let output = with_capture(|| {
            empty_result(true, "No items found.");
        });
        assert_eq!(output.trim(), "[]");
    }

    #[test]
    fn empty_result_human_mode_outputs_message() {
        let output = with_capture(|| {
            empty_result(false, "No items found.");
        });
        assert_eq!(output.trim(), "No items found.");
    }

    #[test]
    fn success_non_tty_no_ansi() {
        let output = with_capture(|| {
            success("Operation complete");
        });
        assert!(
            !output.contains("\x1b["),
            "non-TTY should have no ANSI codes"
        );
        assert!(output.contains("Operation complete"));
    }

    #[test]
    fn warn_non_tty_no_ansi() {
        // In capture mode both stdout and stderr go to the same channel
        let output = with_capture(|| {
            warn("Something may be wrong");
        });
        assert!(
            !output.contains("\x1b["),
            "non-TTY should have no ANSI codes"
        );
        assert!(output.contains("Something may be wrong"));
    }

    #[test]
    fn error_non_tty_plain_prefix() {
        let output = with_capture(|| {
            error("bad input");
        });
        assert!(
            !output.contains("\x1b["),
            "non-TTY should have no ANSI codes"
        );
        assert!(output.contains("error:"));
        assert!(output.contains("bad input"));
    }

    #[test]
    fn section_non_tty_no_bold() {
        let output = with_capture(|| {
            section("My Section Title");
        });
        assert!(
            !output.contains("\x1b["),
            "non-TTY should have no ANSI codes"
        );
        assert!(output.contains("My Section Title"));
    }

    #[test]
    fn kv_formatting() {
        let output = with_capture(|| {
            kv("Status", "healthy");
        });
        assert!(output.contains("Status"));
        assert!(output.contains("healthy"));
        // Key should be left-padded with 2 spaces
        assert!(output.starts_with("  "));
    }

    #[test]
    fn json_output_has_no_ui_artifacts() {
        let data = serde_json::json!({"items": [1, 2, 3]});
        let output = with_capture(|| {
            json_or_table(true, &data, || {});
        });
        assert!(
            !output.contains("\x1b["),
            "JSON output should have no ANSI codes"
        );
        assert!(
            !output.contains("─"),
            "JSON output should have no box-drawing chars"
        );
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert!(parsed.is_object());
    }

    #[test]
    fn table_render_non_tty_no_separator_line() {
        // Use render_to_string(false) to test pipe mode deterministically
        let mut table = CliTable::new(vec!["A", "B"]);
        table.add_row(vec!["1".into(), "hello".into()]);
        let output = table.render_to_string(false);
        assert!(
            !output.contains("─"),
            "non-TTY table should have no separator"
        );
        assert!(output.contains("A"));
        assert!(output.contains("hello"));
    }

    #[test]
    fn table_render_non_tty_no_bold_header() {
        let mut table = CliTable::new(vec!["ID", "NAME"]);
        table.add_row(vec!["1".into(), "Alice".into()]);
        let output = table.render_to_string(false);
        assert!(
            !output.contains("\x1b[1m"),
            "non-TTY table should not bold header"
        );
        assert!(
            !output.contains("\x1b[0m"),
            "non-TTY table should not have reset"
        );
    }

    // ── render_to_string snapshot tests ────────────────────────────────────

    fn sample_projects_table() -> CliTable {
        let mut t = CliTable::new(vec!["ID", "SLUG", "HUMAN_KEY"]);
        t.add_row(vec![
            "1".into(),
            "backend-api".into(),
            "/home/user/projects/backend".into(),
        ]);
        t.add_row(vec![
            "2".into(),
            "frontend".into(),
            "/home/user/projects/frontend".into(),
        ]);
        t
    }

    fn sample_reservations_table() -> CliTable {
        let mut t = CliTable::new(vec!["ID", "PATTERN", "AGENT", "EXPIRES", "REASON"]);
        t.add_row(vec![
            "5".into(),
            "src/auth/**/*.ts".into(),
            "RedBear".into(),
            "2026-02-06T18:00:00".into(),
            "bd-123".into(),
        ]);
        t.add_row(vec![
            "12".into(),
            "src/db/*.rs".into(),
            "GreenCastle".into(),
            "2026-02-06T19:30:00".into(),
            "bd-456".into(),
        ]);
        t
    }

    fn sample_acks_table() -> CliTable {
        let mut t = CliTable::new(vec!["ID", "FROM", "SUBJECT", "IMPORTANCE"]);
        t.add_row(vec![
            "101".into(),
            "GreenCastle".into(),
            "Review needed: auth module".into(),
            "high".into(),
        ]);
        t.add_row(vec![
            "102".into(),
            "BlueLake".into(),
            "Deploy request".into(),
            "urgent".into(),
        ]);
        t
    }

    #[test]
    fn render_to_string_pipe_mode_projects() {
        let table = sample_projects_table();
        let output = table.render_to_string(false);
        assert!(!output.contains('\x1b'), "pipe mode should have no ANSI");
        assert!(!output.contains('─'), "pipe mode should have no separator");
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 3, "header + 2 data rows");
        assert!(lines[0].contains("ID"));
        assert!(lines[0].contains("SLUG"));
        assert!(lines[1].contains("backend-api"));
        assert!(lines[2].contains("frontend"));
    }

    #[test]
    fn render_to_string_tty_mode_projects() {
        let table = sample_projects_table();
        let output = table.render_to_string(true);
        assert!(output.contains("\x1b[1m"), "TTY mode should bold header");
        assert!(
            output.contains("\x1b[0m"),
            "TTY mode should reset after header"
        );
        assert!(output.contains('─'), "TTY mode should have separator");
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 4, "header + separator + 2 data rows");
    }

    #[test]
    fn render_to_string_pipe_mode_reservations() {
        let table = sample_reservations_table();
        let output = table.render_to_string(false);
        assert!(!output.contains('\x1b'));
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("PATTERN"));
        assert!(lines[1].contains("src/auth/**/*.ts"));
        assert!(lines[2].contains("GreenCastle"));
    }

    #[test]
    fn render_to_string_tty_mode_reservations() {
        let table = sample_reservations_table();
        let output = table.render_to_string(true);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 4); // header + sep + 2 rows
        assert!(lines[1].chars().all(|c| c == '─' || c == ' '));
    }

    #[test]
    fn render_to_string_pipe_mode_acks() {
        let table = sample_acks_table();
        let output = table.render_to_string(false);
        assert!(!output.contains('\x1b'));
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("IMPORTANCE"));
        assert!(lines[1].contains("high"));
        assert!(lines[2].contains("urgent"));
    }

    #[test]
    fn render_to_string_tty_mode_acks() {
        let table = sample_acks_table();
        let output = table.render_to_string(true);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 4); // header + sep + 2 rows
        // Separator should span correct width
        let sep = lines[1];
        assert!(sep.contains('─'));
    }

    #[test]
    fn render_to_string_empty_returns_empty() {
        let table = CliTable::new(vec!["A", "B"]);
        assert!(table.render_to_string(false).is_empty());
        assert!(table.render_to_string(true).is_empty());
    }

    #[test]
    fn render_to_string_columns_align_across_rows() {
        let mut t = CliTable::new(vec!["X", "Y"]);
        t.add_row(vec!["short".into(), "a".into()]);
        t.add_row(vec!["very-long-value".into(), "b".into()]);
        let output = t.render_to_string(false);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 3);
        // The second column value should start at the same position in all rows.
        // Column 0 is padded to 15 ("very-long-value"), + 2 spaces gap = col 17.
        let col1_start = "very-long-value".len() + 2;
        for line in &lines {
            if line.len() > col1_start {
                let ch = line.as_bytes()[col1_start];
                assert!(
                    ch != b' ',
                    "column 1 should start at offset {col1_start}: {:?}",
                    line
                );
            }
        }
    }
}
