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
            timestamp_us: 1234567890,
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
}
