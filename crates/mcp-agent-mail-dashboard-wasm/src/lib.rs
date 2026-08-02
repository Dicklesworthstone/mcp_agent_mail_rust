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
#[cfg(feature = "exporter")]
pub mod exporter;
pub mod model;
pub mod runner_core;
pub mod state;
pub mod tui_screens;

#[path = "../../mcp-agent-mail-server/src/tui_screen_registry.rs"]
pub mod tui_screen_registry;

pub use browser_contracts::{
    AgentHealthGrade, AgentHealthMetric, AgentHealthMetricKind, AgentHealthScorecard,
    AnomalySeverity,
};
pub mod evidence_ledger {
    pub use crate::browser_contracts::EvidenceLedgerEntry;
}

#[path = "../../mcp-agent-mail-server/src/tui_chrome.rs"]
pub mod tui_chrome;
#[path = "../../mcp-agent-mail-server/src/tui_events.rs"]
pub mod tui_events;
#[path = "../../mcp-agent-mail-server/src/tui_hit_regions.rs"]
pub mod tui_hit_regions;
#[path = "../../mcp-agent-mail-server/src/tui_layout.rs"]
pub mod tui_layout;
#[path = "../../mcp-agent-mail-server/src/tui_markdown.rs"]
pub mod tui_markdown;
#[path = "../../mcp-agent-mail-server/src/tui_theme.rs"]
pub mod tui_theme;
#[path = "../../mcp-agent-mail-server/src/tui_widgets.rs"]
pub mod tui_widgets;

/// Browser-safe compatibility type consumed by the shared shell renderer.
pub mod tui_persist {
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[allow(clippy::struct_excessive_bools)]
    pub struct AccessibilitySettings {
        pub high_contrast: bool,
        pub key_hints: bool,
        pub reduced_motion: bool,
        pub screen_reader: bool,
    }

    impl Default for AccessibilitySettings {
        fn default() -> Self {
            Self {
                high_contrast: false,
                key_hints: true,
                reduced_motion: false,
                screen_reader: false,
            }
        }
    }
}

/// The shared chrome's help renderer accepts this portable section shape.
/// The browser shell builds a smaller read-only help surface but retains the
/// exact native type contract so the renderer remains one source of truth.
pub mod tui_keymap {
    #[derive(Debug, Clone)]
    pub struct HelpSection {
        pub title: String,
        pub description: Option<String>,
        pub body_markdown: Option<String>,
        pub entries: Vec<(String, String)>,
    }

    impl HelpSection {
        #[must_use]
        pub fn line_count(&self) -> usize {
            let description_lines = usize::from(self.description.is_some());
            let body_lines = self
                .body_markdown
                .as_deref()
                .map_or(0, |body| body.lines().count());
            1 + description_lines + body_lines + self.entries.len()
        }
    }
}

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
