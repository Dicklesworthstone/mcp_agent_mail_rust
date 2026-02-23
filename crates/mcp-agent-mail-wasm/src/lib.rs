//! WASM frontend for MCP Agent Mail TUI dashboard.
//!
//! This crate provides a browser-based interface to the Agent Mail TUI,
//! enabling remote monitoring and interaction with agents via WebSocket.
//!
//! # Architecture
//!
//! ```text
//! Browser (WASM TUI)              MCP Agent Mail Server
//!   ├─ Canvas Terminal  ←──────→  WebSocket State Sync
//!   ├─ Input Handler               ├─ Delta Compression
//!   └─ State Store                 └─ Real-time Updates
//! ```
//!
//! # Building
//!
//! ```bash
//! # Using wasm-pack (recommended)
//! wasm-pack build crates/mcp-agent-mail-wasm --target web
//!
//! # Using cargo directly
//! cargo build --target wasm32-unknown-unknown -p mcp-agent-mail-wasm --release
//! ```
//!
//! # Usage
//!
//! ```javascript
//! import init, { AgentMailApp } from './mcp_agent_mail_wasm.js';
//!
//! async function main() {
//!     await init();
//!     const app = new AgentMailApp('#canvas', 'ws://localhost:8765/ws');
//!     app.start();
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(clippy::pedantic, clippy::nursery)]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(target_arch = "wasm32")]
pub use wasm::*;

// ──────────────────────────────────────────────────────────────────────────────
// Shared types (WASM-compatible)
// ──────────────────────────────────────────────────────────────────────────────

use serde::{Deserialize, Serialize};

/// Application configuration for the WASM TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// WebSocket URL for server connection (e.g., `ws://localhost:8765/ws`)
    pub websocket_url: String,
    /// Canvas element selector (e.g., "#terminal-canvas")
    pub canvas_selector: String,
    /// Enable high-contrast mode for accessibility
    pub high_contrast: bool,
    /// Font size in pixels
    pub font_size_px: u16,
    /// Enable debug overlay
    pub debug_overlay: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            websocket_url: "ws://127.0.0.1:8765/ws".to_string(),
            canvas_selector: "#terminal".to_string(),
            high_contrast: false,
            font_size_px: 14,
            debug_overlay: false,
        }
    }
}

/// Message types for WebSocket communication.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum WsMessage {
    /// Initial state snapshot
    StateSnapshot(StateSnapshot),
    /// Incremental state delta
    StateDelta(StateDelta),
    /// User input event
    Input(InputEvent),
    /// Screen resize event
    Resize { cols: u16, rows: u16 },
    /// Heartbeat/ping
    Ping,
    /// Pong response
    Pong,
}

/// Full state snapshot sent on connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// Current screen ID (1-11)
    pub screen_id: u8,
    /// Screen title
    pub screen_title: String,
    /// Cell grid (serialized)
    pub cells: Vec<u8>,
    /// Grid dimensions
    pub cols: u16,
    pub rows: u16,
    /// Cursor position
    pub cursor_x: u16,
    pub cursor_y: u16,
    /// Whether cursor is visible
    pub cursor_visible: bool,
    /// Server timestamp (microseconds since epoch)
    pub timestamp_us: i64,
}

/// Incremental state update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDelta {
    /// Sequence number for ordering
    pub seq: u64,
    /// Changed cells (sparse update)
    pub changed_cells: Vec<CellChange>,
    /// Optional screen transition
    pub screen_transition: Option<u8>,
    /// Cursor update
    pub cursor: Option<(u16, u16, bool)>,
    /// Server timestamp
    pub timestamp_us: i64,
}

/// Single cell change in a delta update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellChange {
    /// Cell index (row * cols + col)
    pub idx: u32,
    /// Cell data (character + attributes)
    pub data: u32,
}

/// User input event sent to server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum InputEvent {
    /// Key press
    Key { key: String, modifiers: u8 },
    /// Mouse click
    Mouse { x: u16, y: u16, button: u8 },
    /// Mouse scroll
    Scroll { x: u16, y: u16, delta: i8 },
}

/// Runtime synchronization state shared between WASM handlers and rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncState {
    /// Connection flag for UI state.
    pub connected: bool,
    /// Current active screen ID.
    pub screen_id: u8,
    /// Current active screen title.
    pub screen_title: String,
    /// Terminal grid width.
    pub cols: u16,
    /// Terminal grid height.
    pub rows: u16,
    /// Cursor X coordinate.
    pub cursor_x: u16,
    /// Cursor Y coordinate.
    pub cursor_y: u16,
    /// Whether cursor should be rendered.
    pub cursor_visible: bool,
    /// Packed cell values (`u32`) for rendering.
    pub cells: Vec<u32>,
    /// Last processed delta sequence number.
    pub last_seq: u64,
    /// Timestamp from last snapshot/delta.
    pub last_timestamp_us: i64,
    /// Number of inbound state messages processed.
    pub messages_received: u64,
}

impl Default for SyncState {
    fn default() -> Self {
        let cols = 80_u16;
        let rows = 24_u16;
        let capacity = usize::from(cols) * usize::from(rows);
        Self {
            connected: false,
            screen_id: 1,
            screen_title: "Dashboard".to_string(),
            cols,
            rows,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            cells: vec![0; capacity],
            last_seq: 0,
            last_timestamp_us: 0,
            messages_received: 0,
        }
    }
}

impl SyncState {
    fn cell_capacity(&self) -> usize {
        usize::from(self.cols) * usize::from(self.rows)
    }

    fn decode_snapshot_cells(raw: &[u8], expected_len: usize) -> Vec<u32> {
        let mut decoded = if raw.len().is_multiple_of(4) && !raw.is_empty() {
            raw.chunks_exact(4)
                .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect::<Vec<_>>()
        } else {
            raw.iter().map(|&byte| u32::from(byte)).collect::<Vec<_>>()
        };

        if expected_len > 0 {
            decoded.resize(expected_len, 0);
            decoded.truncate(expected_len);
        }
        decoded
    }

    /// Apply a full state snapshot.
    pub fn apply_snapshot(&mut self, snapshot: StateSnapshot) {
        self.screen_id = snapshot.screen_id;
        self.screen_title = snapshot.screen_title;
        self.cols = snapshot.cols;
        self.rows = snapshot.rows;
        self.cursor_x = snapshot.cursor_x;
        self.cursor_y = snapshot.cursor_y;
        self.cursor_visible = snapshot.cursor_visible;
        self.last_timestamp_us = snapshot.timestamp_us;
        self.last_seq = 0;

        let expected = self.cell_capacity();
        self.cells = Self::decode_snapshot_cells(&snapshot.cells, expected);
        self.messages_received = self.messages_received.saturating_add(1);
    }

    /// Apply an incremental delta update.
    pub fn apply_delta(&mut self, delta: StateDelta) {
        self.last_seq = self.last_seq.max(delta.seq);
        self.last_timestamp_us = delta.timestamp_us;

        if let Some(screen_id) = delta.screen_transition {
            self.screen_id = screen_id;
        }
        if let Some((x, y, visible)) = delta.cursor {
            self.cursor_x = x;
            self.cursor_y = y;
            self.cursor_visible = visible;
        }

        let max_idx = delta
            .changed_cells
            .iter()
            .map(|change| (change.idx as usize).saturating_add(1))
            .max()
            .unwrap_or(0);
        let min_capacity = self.cell_capacity().max(max_idx);
        if self.cells.len() < min_capacity {
            self.cells.resize(min_capacity, 0);
        }

        for change in delta.changed_cells {
            let idx = change.idx as usize;
            if idx < self.cells.len() {
                self.cells[idx] = change.data;
            }
        }

        self.messages_received = self.messages_received.saturating_add(1);
    }

    /// Apply an inbound websocket message.
    pub fn apply_message(&mut self, message: &WsMessage) {
        match message {
            WsMessage::StateSnapshot(snapshot) => self.apply_snapshot(snapshot.clone()),
            WsMessage::StateDelta(delta) => self.apply_delta(delta.clone()),
            _ => {}
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Native-only exports (for testing)
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(not(target_arch = "wasm32"))]
pub mod native {
    //! Native utilities for testing WASM types.

    use super::StateSnapshot;

    /// Create a test state snapshot.
    #[must_use]
    pub fn test_snapshot() -> StateSnapshot {
        StateSnapshot {
            screen_id: 1,
            screen_title: "Dashboard".to_string(),
            cells: vec![],
            cols: 80,
            rows: 24,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            timestamp_us: 0,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_serializes_to_json() {
        let config = AppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("websocket_url"));
        assert!(json.contains("canvas_selector"));
    }

    #[test]
    fn ws_message_roundtrip() {
        let msg = WsMessage::Ping;
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: WsMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, WsMessage::Ping));
    }

    #[test]
    fn state_snapshot_serializes() {
        let snap = StateSnapshot {
            screen_id: 1,
            screen_title: "Test".to_string(),
            cells: vec![0, 1, 2],
            cols: 80,
            rows: 24,
            cursor_x: 5,
            cursor_y: 10,
            cursor_visible: true,
            timestamp_us: 1_234_567_890,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("screen_id"));
        assert!(json.contains("Test"));
    }

    #[test]
    fn input_event_variants() {
        let key_event = InputEvent::Key {
            key: "Enter".to_string(),
            modifiers: 0,
        };
        let json = serde_json::to_string(&key_event).unwrap();
        assert!(json.contains("Enter"));

        let mouse_event = InputEvent::Mouse {
            x: 10,
            y: 20,
            button: 0,
        };
        let json = serde_json::to_string(&mouse_event).unwrap();
        assert!(json.contains("Mouse"));
    }

    #[test]
    fn sync_state_apply_snapshot_updates_all_fields() {
        let mut state = SyncState::default();
        let snapshot = StateSnapshot {
            screen_id: 4,
            screen_title: "Agents".to_string(),
            cells: vec![
                1, 0, 0, 0, // 1
                2, 0, 0, 0, // 2
                3, 0, 0, 0, // 3
                4, 0, 0, 0, // 4
            ],
            cols: 2,
            rows: 2,
            cursor_x: 1,
            cursor_y: 1,
            cursor_visible: false,
            timestamp_us: 42,
        };

        state.apply_snapshot(snapshot);

        assert_eq!(state.screen_id, 4);
        assert_eq!(state.screen_title, "Agents");
        assert_eq!(state.cols, 2);
        assert_eq!(state.rows, 2);
        assert_eq!(state.cursor_x, 1);
        assert_eq!(state.cursor_y, 1);
        assert!(!state.cursor_visible);
        assert_eq!(state.cells, vec![1, 2, 3, 4]);
        assert_eq!(state.last_timestamp_us, 42);
        assert_eq!(state.messages_received, 1);
    }

    #[test]
    fn sync_state_apply_delta_mutates_sparse_cells_only() {
        let mut state = SyncState {
            cols: 2,
            rows: 2,
            cells: vec![10, 20, 30, 40],
            ..SyncState::default()
        };
        let delta = StateDelta {
            seq: 9,
            changed_cells: vec![
                CellChange { idx: 1, data: 99 },
                CellChange { idx: 3, data: 77 },
            ],
            screen_transition: Some(6),
            cursor: Some((0, 1, true)),
            timestamp_us: 123,
        };

        state.apply_delta(delta);

        assert_eq!(state.last_seq, 9);
        assert_eq!(state.screen_id, 6);
        assert_eq!(state.cursor_x, 0);
        assert_eq!(state.cursor_y, 1);
        assert!(state.cursor_visible);
        assert_eq!(state.cells, vec![10, 99, 30, 77]);
        assert_eq!(state.last_timestamp_us, 123);
        assert_eq!(state.messages_received, 1);
    }

    #[test]
    fn sync_state_apply_message_tracks_only_state_messages() {
        let mut state = SyncState::default();

        state.apply_message(&WsMessage::Ping);
        assert_eq!(state.messages_received, 0);

        state.apply_message(&WsMessage::StateSnapshot(StateSnapshot {
            screen_id: 2,
            screen_title: "Messages".to_string(),
            cells: vec![0, 0, 0, 0],
            cols: 1,
            rows: 1,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            timestamp_us: 1000,
        }));
        assert_eq!(state.messages_received, 1);

        state.apply_message(&WsMessage::StateDelta(StateDelta {
            seq: 1,
            changed_cells: vec![CellChange { idx: 0, data: 11 }],
            screen_transition: None,
            cursor: None,
            timestamp_us: 1001,
        }));
        assert_eq!(state.messages_received, 2);
        assert_eq!(state.cells, vec![11]);
    }

    // ── decode_snapshot_cells edge cases ──

    #[test]
    fn decode_snapshot_cells_empty_input() {
        let result = SyncState::decode_snapshot_cells(&[], 0);
        assert!(result.is_empty());
    }

    #[test]
    fn decode_snapshot_cells_empty_input_with_expected_len_pads() {
        let result = SyncState::decode_snapshot_cells(&[], 4);
        assert_eq!(result, vec![0, 0, 0, 0]);
    }

    #[test]
    fn decode_snapshot_cells_non_aligned_bytes_fallback() {
        // 3 bytes is not a multiple of 4 — uses byte-level fallback
        let result = SyncState::decode_snapshot_cells(&[10, 20, 30], 3);
        assert_eq!(result, vec![10, 20, 30]);
    }

    #[test]
    fn decode_snapshot_cells_non_aligned_pads_to_expected() {
        let result = SyncState::decode_snapshot_cells(&[1, 2], 5);
        assert_eq!(result, vec![1, 2, 0, 0, 0]);
    }

    #[test]
    fn decode_snapshot_cells_non_aligned_truncates_to_expected() {
        let result = SyncState::decode_snapshot_cells(&[1, 2, 3, 4, 5], 3);
        assert_eq!(result, vec![1, 2, 3]);
    }

    #[test]
    fn decode_snapshot_cells_aligned_le_bytes() {
        // 8 bytes = 2 u32s in little-endian
        let raw = [
            0x01, 0x00, 0x00, 0x00, // 1
            0xFF, 0x00, 0x00, 0x00, // 255
        ];
        let result = SyncState::decode_snapshot_cells(&raw, 2);
        assert_eq!(result, vec![1, 255]);
    }

    #[test]
    fn decode_snapshot_cells_aligned_pads_short_input() {
        // 4 bytes = 1 u32 but expected 3
        let raw = [0x42, 0x00, 0x00, 0x00];
        let result = SyncState::decode_snapshot_cells(&raw, 3);
        assert_eq!(result, vec![0x42, 0, 0]);
    }

    #[test]
    fn decode_snapshot_cells_aligned_truncates_long_input() {
        let raw = [
            0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00,
        ];
        let result = SyncState::decode_snapshot_cells(&raw, 2);
        assert_eq!(result, vec![1, 2]);
    }

    #[test]
    fn decode_snapshot_cells_zero_expected_returns_decoded_as_is() {
        let raw = [0x05, 0x00, 0x00, 0x00];
        let result = SyncState::decode_snapshot_cells(&raw, 0);
        // expected_len=0 skips resize/truncate
        assert_eq!(result, vec![5]);
    }

    // ── apply_delta edge cases ──

    #[test]
    fn apply_delta_empty_changes() {
        let mut state = SyncState {
            cols: 2,
            rows: 2,
            cells: vec![1, 2, 3, 4],
            ..SyncState::default()
        };
        state.apply_delta(StateDelta {
            seq: 5,
            changed_cells: vec![],
            screen_transition: None,
            cursor: None,
            timestamp_us: 100,
        });
        assert_eq!(state.cells, vec![1, 2, 3, 4]);
        assert_eq!(state.last_seq, 5);
        assert_eq!(state.messages_received, 1);
    }

    #[test]
    fn apply_delta_expands_cells_for_out_of_bounds_index() {
        let mut state = SyncState {
            cols: 2,
            rows: 2,
            cells: vec![0, 0, 0, 0],
            ..SyncState::default()
        };
        // idx=7 is beyond 4-cell grid; should expand
        state.apply_delta(StateDelta {
            seq: 1,
            changed_cells: vec![CellChange { idx: 7, data: 42 }],
            screen_transition: None,
            cursor: None,
            timestamp_us: 1,
        });
        assert!(state.cells.len() >= 8);
        assert_eq!(state.cells[7], 42);
    }

    #[test]
    fn apply_delta_seq_uses_max_not_replace() {
        let mut state = SyncState {
            last_seq: 10,
            ..SyncState::default()
        };
        state.apply_delta(StateDelta {
            seq: 5, // older than current
            changed_cells: vec![],
            screen_transition: None,
            cursor: None,
            timestamp_us: 1,
        });
        assert_eq!(state.last_seq, 10, "should keep higher seq");
    }

    #[test]
    fn apply_delta_no_screen_transition_preserves_screen_id() {
        let mut state = SyncState {
            screen_id: 3,
            ..SyncState::default()
        };
        state.apply_delta(StateDelta {
            seq: 1,
            changed_cells: vec![],
            screen_transition: None,
            cursor: None,
            timestamp_us: 1,
        });
        assert_eq!(state.screen_id, 3);
    }

    #[test]
    fn apply_delta_no_cursor_preserves_cursor() {
        let mut state = SyncState {
            cursor_x: 5,
            cursor_y: 10,
            cursor_visible: false,
            ..SyncState::default()
        };
        state.apply_delta(StateDelta {
            seq: 1,
            changed_cells: vec![],
            screen_transition: None,
            cursor: None,
            timestamp_us: 1,
        });
        assert_eq!(state.cursor_x, 5);
        assert_eq!(state.cursor_y, 10);
        assert!(!state.cursor_visible);
    }

    #[test]
    fn apply_delta_duplicate_indices_last_wins() {
        let mut state = SyncState {
            cols: 2,
            rows: 1,
            cells: vec![0, 0],
            ..SyncState::default()
        };
        state.apply_delta(StateDelta {
            seq: 1,
            changed_cells: vec![
                CellChange { idx: 0, data: 10 },
                CellChange { idx: 0, data: 20 },
            ],
            screen_transition: None,
            cursor: None,
            timestamp_us: 1,
        });
        assert_eq!(state.cells[0], 20, "last write to same idx should win");
    }

    // ── apply_snapshot edge cases ──

    #[test]
    fn apply_snapshot_resets_seq_to_zero() {
        let mut state = SyncState {
            last_seq: 99,
            ..SyncState::default()
        };
        state.apply_snapshot(StateSnapshot {
            screen_id: 1,
            screen_title: "Reset".to_string(),
            cells: vec![],
            cols: 1,
            rows: 1,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            timestamp_us: 500,
        });
        assert_eq!(state.last_seq, 0, "snapshot should reset sequence");
    }

    #[test]
    fn apply_snapshot_messages_received_saturates() {
        let mut state = SyncState {
            messages_received: u64::MAX,
            ..SyncState::default()
        };
        state.apply_snapshot(StateSnapshot {
            screen_id: 1,
            screen_title: "Sat".to_string(),
            cells: vec![],
            cols: 1,
            rows: 1,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            timestamp_us: 0,
        });
        assert_eq!(
            state.messages_received,
            u64::MAX,
            "should saturate, not overflow"
        );
    }

    // ── apply_message edge cases ──

    #[test]
    fn apply_message_resize_is_ignored() {
        let mut state = SyncState::default();
        state.apply_message(&WsMessage::Resize {
            cols: 120,
            rows: 40,
        });
        assert_eq!(state.messages_received, 0);
        assert_eq!(state.cols, 80, "resize message should not alter state");
    }

    #[test]
    fn apply_message_pong_is_ignored() {
        let mut state = SyncState::default();
        state.apply_message(&WsMessage::Pong);
        assert_eq!(state.messages_received, 0);
    }

    // ── WsMessage serde edge cases ──

    #[test]
    fn ws_message_resize_roundtrip() {
        let msg = WsMessage::Resize {
            cols: 120,
            rows: 40,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: WsMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            WsMessage::Resize {
                cols: 120,
                rows: 40
            }
        ));
    }

    #[test]
    fn ws_message_pong_roundtrip() {
        let msg = WsMessage::Pong;
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: WsMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, WsMessage::Pong));
    }

    #[test]
    fn ws_message_state_delta_roundtrip() {
        let delta = StateDelta {
            seq: 42,
            changed_cells: vec![CellChange { idx: 0, data: 7 }],
            screen_transition: Some(3),
            cursor: Some((1, 2, false)),
            timestamp_us: 999,
        };
        let msg = WsMessage::StateDelta(delta);
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: WsMessage = serde_json::from_str(&json).unwrap();
        if let WsMessage::StateDelta(d) = parsed {
            assert_eq!(d.seq, 42);
            assert_eq!(d.changed_cells.len(), 1);
            assert_eq!(d.screen_transition, Some(3));
            assert_eq!(d.cursor, Some((1, 2, false)));
        } else {
            panic!("expected StateDelta variant");
        }
    }

    #[test]
    fn scroll_input_event_roundtrip() {
        let event = InputEvent::Scroll {
            x: 5,
            y: 10,
            delta: -3,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: InputEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            InputEvent::Scroll {
                x: 5,
                y: 10,
                delta: -3
            }
        ));
    }

    // ── SyncState default ──

    #[test]
    fn sync_state_default_has_correct_dimensions() {
        let state = SyncState::default();
        assert_eq!(state.cols, 80);
        assert_eq!(state.rows, 24);
        assert_eq!(state.cells.len(), 80 * 24);
        assert!(state.cells.iter().all(|&c| c == 0));
        assert_eq!(state.screen_id, 1);
        assert_eq!(state.screen_title, "Dashboard");
        assert!(state.cursor_visible);
        assert_eq!(state.last_seq, 0);
        assert_eq!(state.messages_received, 0);
    }

    // ── AppConfig default ──

    #[test]
    fn app_config_default_values() {
        let config = AppConfig::default();
        assert_eq!(config.websocket_url, "ws://127.0.0.1:8765/ws");
        assert_eq!(config.canvas_selector, "#terminal");
        assert!(!config.high_contrast);
        assert_eq!(config.font_size_px, 14);
        assert!(!config.debug_overlay);
    }

    #[test]
    fn app_config_serde_roundtrip() {
        let config = AppConfig {
            websocket_url: "ws://custom:9999/ws".to_string(),
            canvas_selector: "#custom".to_string(),
            high_contrast: true,
            font_size_px: 20,
            debug_overlay: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.websocket_url, "ws://custom:9999/ws");
        assert!(parsed.high_contrast);
        assert_eq!(parsed.font_size_px, 20);
        assert!(parsed.debug_overlay);
    }

    // ── native test helper ──

    #[test]
    fn native_test_snapshot_has_expected_defaults() {
        let snap = native::test_snapshot();
        assert_eq!(snap.screen_id, 1);
        assert_eq!(snap.screen_title, "Dashboard");
        assert!(snap.cells.is_empty());
        assert_eq!(snap.cols, 80);
        assert_eq!(snap.rows, 24);
        assert_eq!(snap.timestamp_us, 0);
    }
}
