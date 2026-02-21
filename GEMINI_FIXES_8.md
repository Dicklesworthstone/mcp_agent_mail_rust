# Gemini Fixes Report

## TUI Truncation Fixes

I identified and fixed critical visual layout bugs related to string truncation in the TUI dashboard and message browser. The original implementations were using byte-based length calculations (`s.len()`, `s.chars().take(n)`) instead of visual column width (`display_width`). This caused broken layouts and premature truncation when displaying Unicode content (emoji, CJK characters, or multi-byte symbols like the selection marker `▶`).

### 1. Message Browser (`messages.rs`)

*   **Fixed Selection Marker Width:** Updated `MessageEntry::render_row` to calculate `fixed_len` using `ftui::text::display_width(marker)` instead of `marker.len()`. The selection marker `▶ ` is 4 bytes but only 2 columns wide. The previous code overestimated the width by 2, causing the subject line to be truncated 2 columns too early when selected.
*   **Fixed `truncate_str`:** Replaced the naive character-counting implementation with a column-aware version using `crate::tui_widgets::truncate_width` and `ftui::text::display_width`. It now correctly reserves visual space for the `...` ellipsis and handles wide characters without breaking layout.

### 2. Dashboard (`dashboard.rs`)

*   **Fixed `truncate`:** Replaced the byte-slicing implementation with `crate::tui_widgets::truncate_width`. This ensures that panel hints and titles containing Unicode (e.g. project slugs) are truncated correctly according to their visual display width, preventing alignment issues in the dashboard grid.

These changes ensure the TUI remains robust and visually consistent across all supported terminal emulators and with diverse content.
