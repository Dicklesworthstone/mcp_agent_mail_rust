#![forbid(unsafe_code)]

//! Browser-safe runner for the real Agent Mail FrankenTUI dashboard.
//!
//! The dashboard source itself is shared with `mcp-agent-mail-server`; this
//! crate supplies a portable state adapter and a host-driven `ftui-web`
//! runner. No database, HTTP server, filesystem, TTY, or mailbox mutation
//! dependency enters the browser build.

// The shared TUI modules refer to a small set of operator-facing core DTOs by
// crate-qualified path. This browser crate supplies the same serialized
// contracts without linking the native filesystem/configuration core.
extern crate self as mcp_agent_mail_core;

mod browser_contracts;
pub mod console;
pub mod demo_pack;
pub mod model;
pub mod runner_core;
pub mod state;
pub mod tui_screens;

pub use browser_contracts::{
    AgentHealthGrade, AgentHealthMetric, AgentHealthMetricKind, AgentHealthScorecard,
    AnomalySeverity,
};
pub mod evidence_ledger {
    pub use crate::browser_contracts::EvidenceLedgerEntry;
}

#[path = "../../mcp-agent-mail-server/src/tui_events.rs"]
pub mod tui_events;
#[path = "../../mcp-agent-mail-server/src/tui_layout.rs"]
pub mod tui_layout;
#[path = "../../mcp-agent-mail-server/src/tui_markdown.rs"]
pub mod tui_markdown;
#[path = "../../mcp-agent-mail-server/src/tui_theme.rs"]
pub mod tui_theme;
#[path = "../../mcp-agent-mail-server/src/tui_widgets.rs"]
pub mod tui_widgets;

/// Compatibility namespace expected by the shared dashboard source. The
/// concrete type is a browser-safe operator-state adapter, not the native
/// server bridge.
pub mod tui_bridge {
    pub use crate::state::{
        ConfigSnapshot, RequestCounters, ScreenDiagnosticSnapshot, TuiSharedState,
    };
}

pub use demo_pack::{DemoPack, DemoPackError, curated_public_demo};
pub use runner_core::{DashboardRunnerCore, RunnerStatus};

#[cfg(target_arch = "wasm32")]
mod wasm;
