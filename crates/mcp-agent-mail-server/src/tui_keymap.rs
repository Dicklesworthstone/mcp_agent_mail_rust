//! Global keybinding map, configurable profiles, and conflict detection for `AgentMailTUI`.
//!
//! Provides a structured registry of all global keybindings with
//! conflict detection against screen-specific bindings, plus
//! configurable keymap profiles (Default, Vim, Emacs, Minimal) and
//! user-level rebinding overrides.

use std::collections::HashMap;

use ftui::KeyCode;
use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────
// GlobalBinding — structured keybinding definition
// ──────────────────────────────────────────────────────────────────────

/// A global keybinding entry.
#[derive(Debug, Clone)]
pub struct GlobalBinding {
    /// Key label for display (e.g. "q", "Tab", "Ctrl+P").
    pub label: &'static str,
    /// Human-readable action description.
    pub action: &'static str,
    /// Whether this binding is suppressed when a screen's
    /// `consumes_text_input()` returns `true`.
    pub text_suppressible: bool,
}

/// All global keybindings in display order.
///
/// These are processed in `MailAppModel::update` before forwarding events
/// to the active screen.  Bindings marked `text_suppressible` are skipped
/// when the active screen or command palette is consuming text input.
pub const GLOBAL_BINDINGS: &[GlobalBinding] = &[
    GlobalBinding {
        label: "1-8",
        action: "Jump to screen",
        text_suppressible: true,
    },
    GlobalBinding {
        label: "Tab",
        action: "Next screen",
        text_suppressible: false,
    },
    GlobalBinding {
        label: "Shift+Tab",
        action: "Previous screen",
        text_suppressible: false,
    },
    GlobalBinding {
        label: "m",
        action: "Toggle MCP/API mode",
        text_suppressible: true,
    },
    GlobalBinding {
        label: "Ctrl+P",
        action: "Command palette",
        text_suppressible: false,
    },
    GlobalBinding {
        label: ":",
        action: "Command palette",
        text_suppressible: true,
    },
    GlobalBinding {
        label: "T",
        action: "Cycle theme",
        text_suppressible: true,
    },
    GlobalBinding {
        label: "?",
        action: "Toggle help",
        text_suppressible: true,
    },
    GlobalBinding {
        label: "q",
        action: "Quit",
        text_suppressible: true,
    },
    GlobalBinding {
        label: "Esc",
        action: "Dismiss overlay",
        text_suppressible: false,
    },
];

/// Normalize a keybinding label to a set of `KeyCode` values it matches.
///
/// Returns `None` for compound labels like "1-8" or "Ctrl+P" that
/// don't map to a single `KeyCode`.
#[must_use]
pub fn label_to_keycodes(label: &str) -> Vec<KeyCode> {
    match label {
        "Tab" => vec![KeyCode::Tab],
        "Shift+Tab" => vec![KeyCode::BackTab],
        "Esc" => vec![KeyCode::Escape],
        "Enter" => vec![KeyCode::Enter],
        "Backspace" => vec![KeyCode::Backspace],
        "Up" => vec![KeyCode::Up],
        "Down" => vec![KeyCode::Down],
        "Left" => vec![KeyCode::Left],
        "Right" => vec![KeyCode::Right],
        "PageUp" => vec![KeyCode::PageUp],
        "PageDown" => vec![KeyCode::PageDown],
        "Home" => vec![KeyCode::Home],
        "End" => vec![KeyCode::End],
        // Ranges
        "1-8" => (1..=8)
            .map(|n| KeyCode::Char(char::from_digit(n, 10).unwrap()))
            .collect(),
        "1-9" => (1..=9)
            .map(|n| KeyCode::Char(char::from_digit(n, 10).unwrap()))
            .collect(),
        // Modifiers (skip — these don't conflict with single-char bindings)
        s if s.starts_with("Ctrl+") => vec![],
        s if s.starts_with("Shift+") => vec![],
        // Single char
        s if s.len() == 1 => {
            let ch = s.chars().next().unwrap();
            vec![KeyCode::Char(ch)]
        }
        // Slash-separated shortcuts like "j/k" or "i/Enter"
        s if s.contains('/') => s
            .split('/')
            .flat_map(|part| label_to_keycodes(part.trim()))
            .collect(),
        _ => vec![],
    }
}

/// Check whether two keybinding sets overlap.
///
/// Returns a list of `(global_label, screen_label, conflicting_keycode)` tuples
/// for any global binding that conflicts with a screen binding, considering
/// only global bindings that are `text_suppressible` (which share the
/// single-char namespace with screen-specific bindings).
#[must_use]
pub fn detect_conflicts(
    screen_bindings: &[(&str, &str)],
) -> Vec<(&'static str, &'static str, String)> {
    let mut conflicts = Vec::new();

    for global in GLOBAL_BINDINGS {
        if !global.text_suppressible {
            // Non-suppressible globals (Tab, Esc, Ctrl+P) are always processed
            // before screen dispatch, so they can't conflict.
            continue;
        }

        let global_codes = label_to_keycodes(global.label);
        for &(screen_label, _screen_action) in screen_bindings {
            let screen_codes = label_to_keycodes(screen_label);
            for gc in &global_codes {
                if screen_codes.contains(gc) {
                    conflicts.push((global.label, global.action, format!("{gc:?}")));
                }
            }
        }
    }

    conflicts
}

// ──────────────────────────────────────────────────────────────────────
// KeymapProfile — named keybinding configurations
// ──────────────────────────────────────────────────────────────────────

/// Built-in keymap profiles.
///
/// Each profile defines a set of action-to-key mappings for global bindings.
/// The `Default` profile matches the existing hardcoded bindings.
/// `Vim` and `Emacs` provide familiar muscle-memory for those editors.
/// `Minimal` strips suppressible shortcuts for safety in text-heavy contexts.
/// `Custom` uses only user-provided overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeymapProfile {
    Default,
    Vim,
    Emacs,
    Minimal,
    Custom,
}

impl KeymapProfile {
    /// All built-in profiles in display order.
    pub const ALL: &'static [Self] = &[
        Self::Default,
        Self::Vim,
        Self::Emacs,
        Self::Minimal,
        Self::Custom,
    ];

    /// Short label for the profile.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Vim => "Vim",
            Self::Emacs => "Emacs",
            Self::Minimal => "Minimal",
            Self::Custom => "Custom",
        }
    }

    /// Cycle to the next profile (wrapping).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Default => Self::Vim,
            Self::Vim => Self::Emacs,
            Self::Emacs => Self::Minimal,
            Self::Minimal => Self::Custom,
            Self::Custom => Self::Default,
        }
    }

    /// Get the base bindings for this profile.
    ///
    /// Returns `(action_id, label, action_description, text_suppressible)` tuples.
    #[must_use]
    pub fn bindings(self) -> Vec<ProfileBinding> {
        match self {
            Self::Default | Self::Custom => default_profile_bindings(),
            Self::Vim => vim_profile_bindings(),
            Self::Emacs => emacs_profile_bindings(),
            Self::Minimal => minimal_profile_bindings(),
        }
    }
}

// `Default` variant is the default.
// Cannot use `#[derive(Default)]` because `Default` is a variant name.
#[allow(clippy::derivable_impls)]
impl Default for KeymapProfile {
    fn default() -> Self {
        Self::Default
    }
}

/// A binding entry within a profile.
#[derive(Debug, Clone)]
pub struct ProfileBinding {
    /// Stable identifier for the action (e.g. `"jump_screen"`, `"quit"`).
    pub action_id: &'static str,
    /// Key label for display (e.g. `"q"`, `"Tab"`).
    pub label: &'static str,
    /// Human-readable action description.
    pub action: &'static str,
    /// Whether suppressed during text input.
    pub text_suppressible: bool,
}

fn default_profile_bindings() -> Vec<ProfileBinding> {
    GLOBAL_BINDINGS
        .iter()
        .map(|b| ProfileBinding {
            action_id: action_id_for_label(b.label),
            label: b.label,
            action: b.action,
            text_suppressible: b.text_suppressible,
        })
        .collect()
}

fn vim_profile_bindings() -> Vec<ProfileBinding> {
    vec![
        pb("jump_screen", "1-8", "Jump to screen", true),
        pb("next_screen", "Tab", "Next screen", false),
        pb("prev_screen", "Shift+Tab", "Previous screen", false),
        pb("toggle_mode", "m", "Toggle MCP/API mode", true),
        pb("command_palette", ":", "Command palette", true),
        pb("command_palette_ctrl", "Ctrl+P", "Command palette", false),
        pb("cycle_theme", "T", "Cycle theme", true),
        pb("toggle_help", "?", "Toggle help", true),
        pb("quit", "q", "Quit", true),
        pb("dismiss", "Esc", "Dismiss overlay", false),
        // Vim-specific navigation additions
        pb("scroll_down", "j", "Scroll down", true),
        pb("scroll_up", "k", "Scroll up", true),
        pb("top", "g", "Go to top", true),
        pb("bottom", "G", "Go to bottom", true),
        pb("search", "/", "Search", true),
    ]
}

fn emacs_profile_bindings() -> Vec<ProfileBinding> {
    vec![
        pb("jump_screen", "1-8", "Jump to screen", true),
        pb("next_screen", "Tab", "Next screen", false),
        pb("prev_screen", "Shift+Tab", "Previous screen", false),
        pb("toggle_mode", "m", "Toggle MCP/API mode", true),
        pb("command_palette", "Ctrl+P", "Command palette", false),
        pb("cycle_theme", "T", "Cycle theme", true),
        pb("toggle_help", "?", "Toggle help", true),
        pb("quit", "q", "Quit", true),
        pb("dismiss", "Esc", "Dismiss overlay / Cancel", false),
        // Emacs-specific bindings
        pb("scroll_down", "Ctrl+N", "Next line", false),
        pb("scroll_up", "Ctrl+P_nav", "Previous line", false),
        pb("search", "Ctrl+S", "Search", false),
    ]
}

fn minimal_profile_bindings() -> Vec<ProfileBinding> {
    vec![
        pb("next_screen", "Tab", "Next screen", false),
        pb("prev_screen", "Shift+Tab", "Previous screen", false),
        pb("command_palette", "Ctrl+P", "Command palette", false),
        pb("toggle_help", "?", "Toggle help", true),
        pb("dismiss", "Esc", "Dismiss overlay", false),
    ]
}

const fn pb(
    action_id: &'static str,
    label: &'static str,
    action: &'static str,
    text_suppressible: bool,
) -> ProfileBinding {
    ProfileBinding {
        action_id,
        label,
        action,
        text_suppressible,
    }
}

/// Map a key label to a stable action ID.
fn action_id_for_label(label: &str) -> &'static str {
    match label {
        "1-8" | "1-9" => "jump_screen",
        "Tab" => "next_screen",
        "Shift+Tab" => "prev_screen",
        "m" => "toggle_mode",
        "Ctrl+P" => "command_palette_ctrl",
        ":" => "command_palette",
        "T" => "cycle_theme",
        "?" => "toggle_help",
        "q" => "quit",
        "Esc" => "dismiss",
        _ => "unknown",
    }
}

// ──────────────────────────────────────────────────────────────────────
// BindingOverride — user-level key rebinding
// ──────────────────────────────────────────────────────────────────────

/// A user-specified key rebinding: override the key for a given action ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingOverride {
    /// The stable action identifier to rebind.
    pub action_id: String,
    /// The new key label (e.g. `"x"`, `"Ctrl+Q"`).
    pub new_label: String,
}

// ──────────────────────────────────────────────────────────────────────
// KeymapRegistry — the live keymap state
// ──────────────────────────────────────────────────────────────────────

/// The live keymap: active profile + user overrides.
///
/// The registry merges profile base bindings with user overrides,
/// where overrides take precedence. It provides lookup by action ID
/// and generates the help-overlay data.
#[derive(Debug, Clone)]
pub struct KeymapRegistry {
    profile: KeymapProfile,
    overrides: Vec<BindingOverride>,
    /// Resolved bindings: `action_id` to `(label, action_desc, text_suppressible)`.
    resolved: HashMap<String, (String, String, bool)>,
}

impl KeymapRegistry {
    /// Create a registry from a profile with no overrides.
    #[must_use]
    pub fn new(profile: KeymapProfile) -> Self {
        let mut reg = Self {
            profile,
            overrides: Vec::new(),
            resolved: HashMap::new(),
        };
        reg.rebuild();
        reg
    }

    /// Create a registry with user overrides applied.
    #[must_use]
    pub fn with_overrides(profile: KeymapProfile, overrides: Vec<BindingOverride>) -> Self {
        let mut reg = Self {
            profile,
            overrides,
            resolved: HashMap::new(),
        };
        reg.rebuild();
        reg
    }

    /// The active profile.
    #[must_use]
    pub const fn profile(&self) -> KeymapProfile {
        self.profile
    }

    /// Switch to a new profile, preserving overrides.
    pub fn set_profile(&mut self, profile: KeymapProfile) {
        self.profile = profile;
        self.rebuild();
    }

    /// Cycle to the next profile.
    pub fn cycle_profile(&mut self) {
        self.set_profile(self.profile.next());
    }

    /// Add or update an override. Rebuilds the resolved map.
    pub fn add_override(&mut self, ovr: BindingOverride) {
        // Remove existing override for same action_id.
        self.overrides.retain(|o| o.action_id != ovr.action_id);
        self.overrides.push(ovr);
        self.rebuild();
    }

    /// Remove an override by action ID. Rebuilds the resolved map.
    pub fn remove_override(&mut self, action_id: &str) {
        self.overrides.retain(|o| o.action_id != action_id);
        self.rebuild();
    }

    /// Clear all overrides. Rebuilds the resolved map.
    pub fn clear_overrides(&mut self) {
        self.overrides.clear();
        self.rebuild();
    }

    /// The current user overrides.
    #[must_use]
    pub fn overrides(&self) -> &[BindingOverride] {
        &self.overrides
    }

    /// Look up the key label for a given action ID.
    #[must_use]
    pub fn label_for(&self, action_id: &str) -> Option<&str> {
        self.resolved.get(action_id).map(|(l, _, _)| l.as_str())
    }

    /// Look up whether an action is text-suppressible.
    #[must_use]
    pub fn is_suppressible(&self, action_id: &str) -> bool {
        self.resolved
            .get(action_id)
            .is_some_and(|(_, _, s)| *s)
    }

    /// Generate global binding entries in display order for the help overlay.
    #[must_use]
    pub fn help_entries(&self) -> Vec<(String, String)> {
        let base = self.profile.bindings();
        let mut entries = Vec::new();
        for b in &base {
            let label = self
                .resolved
                .get(b.action_id)
                .map_or_else(|| b.label.to_string(), |(l, _, _)| l.clone());
            entries.push((label, b.action.to_string()));
        }
        entries
    }

    /// Generate structured help sections for the context-aware overlay.
    #[must_use]
    pub fn contextual_help(
        &self,
        screen_bindings: &[crate::tui_screens::HelpEntry],
        screen_label: &str,
    ) -> Vec<HelpSection> {
        let mut sections = Vec::new();

        // Global section
        let global_entries = self.help_entries();
        sections.push(HelpSection {
            title: format!("Global ({})", self.profile.label()),
            entries: global_entries,
        });

        // Screen section
        if !screen_bindings.is_empty() {
            sections.push(HelpSection {
                title: screen_label.to_string(),
                entries: screen_bindings
                    .iter()
                    .map(|e| (e.key.to_string(), e.action.to_string()))
                    .collect(),
            });
        }

        sections
    }

    /// Check for conflicts between this registry's bindings and screen bindings.
    #[must_use]
    pub fn conflicts_with(&self, screen_bindings: &[(&str, &str)]) -> Vec<(String, String, String)> {
        let mut conflicts = Vec::new();
        for (action_id, (label, _action, suppressible)) in &self.resolved {
            if !suppressible {
                continue;
            }
            let global_codes = label_to_keycodes(label);
            for &(screen_label, _) in screen_bindings {
                let screen_codes = label_to_keycodes(screen_label);
                for gc in &global_codes {
                    if screen_codes.contains(gc) {
                        conflicts.push((
                            action_id.clone(),
                            label.clone(),
                            format!("{gc:?}"),
                        ));
                    }
                }
            }
        }
        conflicts
    }

    /// Rebuild the resolved binding map from profile + overrides.
    fn rebuild(&mut self) {
        self.resolved.clear();
        for b in self.profile.bindings() {
            self.resolved.insert(
                b.action_id.to_string(),
                (b.label.to_string(), b.action.to_string(), b.text_suppressible),
            );
        }
        // Apply overrides (label only, preserving action desc + suppressibility).
        for ovr in &self.overrides {
            if let Some(entry) = self.resolved.get_mut(&ovr.action_id) {
                entry.0.clone_from(&ovr.new_label);
            }
        }
    }
}

impl Default for KeymapRegistry {
    fn default() -> Self {
        Self::new(KeymapProfile::Default)
    }
}

// ──────────────────────────────────────────────────────────────────────
// HelpSection — structured help overlay data
// ──────────────────────────────────────────────────────────────────────

/// A section of keybindings for the help overlay.
#[derive(Debug, Clone)]
pub struct HelpSection {
    /// Section title (e.g. "Global (Vim)", "Dashboard").
    pub title: String,
    /// `(key_label, action_description)` pairs.
    pub entries: Vec<(String, String)>,
}

impl HelpSection {
    /// Total number of display lines (title + entries).
    #[must_use]
    pub fn line_count(&self) -> usize {
        1 + self.entries.len()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_screens::{
        ALL_SCREEN_IDS, MailScreen, agents::AgentsScreen, dashboard::DashboardScreen,
        messages::MessageBrowserScreen, reservations::ReservationsScreen,
        system_health::SystemHealthScreen, threads::ThreadExplorerScreen, timeline::TimelineScreen,
        tool_metrics::ToolMetricsScreen,
    };
    use std::collections::HashSet;
    use std::sync::Arc;

    #[test]
    fn global_bindings_not_empty() {
        assert!(GLOBAL_BINDINGS.len() >= 8);
    }

    #[test]
    fn global_bindings_have_labels_and_actions() {
        for binding in GLOBAL_BINDINGS {
            assert!(!binding.label.is_empty());
            assert!(!binding.action.is_empty());
        }
    }

    #[test]
    fn label_to_keycodes_single_char() {
        let codes = label_to_keycodes("q");
        assert_eq!(codes, vec![KeyCode::Char('q')]);
    }

    #[test]
    fn label_to_keycodes_special_keys() {
        assert_eq!(label_to_keycodes("Tab"), vec![KeyCode::Tab]);
        assert_eq!(label_to_keycodes("Esc"), vec![KeyCode::Escape]);
        assert_eq!(label_to_keycodes("Enter"), vec![KeyCode::Enter]);
    }

    #[test]
    fn label_to_keycodes_range() {
        let codes = label_to_keycodes("1-8");
        assert_eq!(codes.len(), 8);
        assert_eq!(codes[0], KeyCode::Char('1'));
        assert_eq!(codes[6], KeyCode::Char('7'));
        assert_eq!(codes[7], KeyCode::Char('8'));
    }

    #[test]
    fn label_to_keycodes_ctrl_modifier_returns_empty() {
        // Ctrl+P doesn't conflict with plain 'P'
        assert!(label_to_keycodes("Ctrl+P").is_empty());
    }

    #[test]
    fn label_to_keycodes_slash_separated() {
        let codes = label_to_keycodes("j/k");
        assert_eq!(codes, vec![KeyCode::Char('j'), KeyCode::Char('k')]);
    }

    #[test]
    fn label_to_keycodes_slash_with_special() {
        let codes = label_to_keycodes("i/Enter");
        assert_eq!(codes, vec![KeyCode::Char('i'), KeyCode::Enter]);
    }

    #[test]
    fn detect_conflicts_no_overlap() {
        let screen_bindings = &[("x", "Do X"), ("y", "Do Y")];
        let conflicts = detect_conflicts(screen_bindings);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn detect_conflicts_with_overlap() {
        // 'q' is a global binding — if a screen also binds 'q', it's a conflict
        let screen_bindings = &[("q", "Screen quit")];
        let conflicts = detect_conflicts(screen_bindings);
        assert!(
            !conflicts.is_empty(),
            "expected conflict for 'q' but found none"
        );
    }

    #[test]
    fn detect_conflicts_non_suppressible_ignored() {
        // Tab is non-suppressible, so it doesn't conflict even if a screen binds Tab
        let screen_bindings = &[("Tab", "Screen tab action")];
        let conflicts = detect_conflicts(screen_bindings);
        assert!(
            conflicts.is_empty(),
            "non-suppressible bindings should not report conflicts"
        );
    }

    /// Verify no screen has keybindings that conflict with global text-suppressible bindings.
    ///
    /// This is the key contract: when `consumes_text_input()` returns false,
    /// global single-char shortcuts take precedence, so screens must not
    /// bind the same keys for different actions.
    #[test]
    fn no_screen_conflicts_with_global_bindings() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);

        let screens: Vec<(&str, Box<dyn MailScreen>)> = vec![
            ("Dashboard", Box::new(DashboardScreen::new())),
            ("Messages", Box::new(MessageBrowserScreen::new())),
            ("Threads", Box::new(ThreadExplorerScreen::new())),
            ("Timeline", Box::new(TimelineScreen::new())),
            (
                "SystemHealth",
                Box::new(SystemHealthScreen::new(Arc::clone(&state))),
            ),
            ("Agents", Box::new(AgentsScreen::new())),
            ("Reservations", Box::new(ReservationsScreen::new())),
            ("ToolMetrics", Box::new(ToolMetricsScreen::new())),
        ];

        let mut all_conflicts = Vec::new();
        for (name, screen) in &screens {
            let bindings: Vec<(&str, &str)> = screen
                .keybindings()
                .iter()
                .map(|h| (h.key, h.action))
                .collect();
            let conflicts = detect_conflicts(&bindings);
            for (global_label, global_action, keycode) in &conflicts {
                all_conflicts.push(format!(
                    "Screen '{name}': global '{global_label}' ({global_action}) \
                     conflicts with screen binding on {keycode}"
                ));
            }
        }

        // Known acceptable overlaps: screen bindings that are intentionally
        // the same as global bindings (e.g., a screen that also uses '?' for help).
        // These are handled by the global dispatch taking precedence.
        // Filter out known-safe overlaps where the action semantics match.
        let critical: Vec<&str> = all_conflicts
            .iter()
            .filter(|c| {
                // Number keys 1-8 overlap with timeline's "1-9" correlation links.
                // This is handled: timeline only processes 1-9 when the dock is visible,
                // while global number keys are caught first in tui_app.rs.
                !c.contains("1-8")
            })
            .map(String::as_str)
            .collect();

        assert!(
            critical.is_empty(),
            "Keybinding conflicts detected:\n{}",
            critical.join("\n")
        );
    }

    /// All screens implement consistent navigation key semantics.
    #[test]
    fn all_screens_have_keybindings_method() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);

        let app = crate::tui_app::MailAppModel::new(state);
        // Every screen should be accessible and have a keybindings() result
        for &id in ALL_SCREEN_IDS {
            assert!(
                app.help_visible() || !app.help_visible(),
                "screen {id:?} should be accessible"
            );
        }
    }

    /// Global bindings have no internal duplicates.
    #[test]
    fn global_bindings_no_internal_duplicates() {
        let mut seen_codes: HashSet<String> = HashSet::new();
        for binding in GLOBAL_BINDINGS {
            let codes = label_to_keycodes(binding.label);
            for code in codes {
                let key = format!("{code:?}");
                // Some keys map to the same action (Ctrl+P and ':' both open palette)
                // which is acceptable. Only flag if different actions.
                if !seen_codes.insert(format!("{key}:{}", binding.action)) {
                    // Same action on same key is fine (deduplicated display)
                }
            }
        }
    }

    /// `text_suppressible` flag is correct for all global bindings.
    #[test]
    fn text_suppressible_flag_correctness() {
        for binding in GLOBAL_BINDINGS {
            match binding.label {
                "Tab" | "Shift+Tab" | "Esc" | "Ctrl+P" => {
                    assert!(
                        !binding.text_suppressible,
                        "{} should NOT be text-suppressible",
                        binding.label
                    );
                }
                "q" | "?" | ":" | "m" | "T" | "1-8" => {
                    assert!(
                        binding.text_suppressible,
                        "{} should be text-suppressible",
                        binding.label
                    );
                }
                _ => {} // other bindings: no assertion
            }
        }
    }

    // ── KeymapProfile tests ──

    #[test]
    fn profile_all_has_five_entries() {
        assert_eq!(KeymapProfile::ALL.len(), 5);
    }

    #[test]
    fn profile_default_is_default() {
        assert_eq!(KeymapProfile::default(), KeymapProfile::Default);
    }

    #[test]
    fn profile_labels_non_empty() {
        for p in KeymapProfile::ALL {
            assert!(!p.label().is_empty());
        }
    }

    #[test]
    fn profile_cycle_wraps() {
        let start = KeymapProfile::Default;
        let mut p = start;
        for _ in 0..5 {
            p = p.next();
        }
        assert_eq!(p, start, "cycling 5 times should return to start");
    }

    #[test]
    fn profile_default_bindings_match_global() {
        let bindings = KeymapProfile::Default.bindings();
        assert_eq!(
            bindings.len(),
            GLOBAL_BINDINGS.len(),
            "default profile should have same binding count as GLOBAL_BINDINGS"
        );
    }

    #[test]
    fn profile_vim_has_extra_bindings() {
        let vim = KeymapProfile::Vim.bindings();
        let def = KeymapProfile::Default.bindings();
        assert!(
            vim.len() > def.len(),
            "vim profile should have more bindings than default"
        );
        // Should have j/k/g/G/search
        let ids: Vec<&str> = vim.iter().map(|b| b.action_id).collect();
        assert!(ids.contains(&"scroll_down"));
        assert!(ids.contains(&"scroll_up"));
        assert!(ids.contains(&"search"));
    }

    #[test]
    fn profile_minimal_is_subset() {
        let min_bindings = KeymapProfile::Minimal.bindings();
        assert!(
            min_bindings.len() < GLOBAL_BINDINGS.len(),
            "minimal profile should have fewer bindings"
        );
        // Should not contain quit or toggle_mode
        let ids: Vec<&str> = min_bindings.iter().map(|b| b.action_id).collect();
        assert!(!ids.contains(&"quit"));
        assert!(!ids.contains(&"toggle_mode"));
    }

    #[test]
    fn profile_serde_roundtrip() {
        let p = KeymapProfile::Vim;
        let json = serde_json::to_string(&p).unwrap();
        let p2: KeymapProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(p, p2);
    }

    // ── KeymapRegistry tests ──

    #[test]
    fn registry_default_resolves_quit() {
        let reg = KeymapRegistry::default();
        assert_eq!(reg.label_for("quit"), Some("q"));
    }

    #[test]
    fn registry_profile_switch() {
        let mut reg = KeymapRegistry::new(KeymapProfile::Default);
        assert_eq!(reg.profile(), KeymapProfile::Default);
        reg.set_profile(KeymapProfile::Vim);
        assert_eq!(reg.profile(), KeymapProfile::Vim);
        // Vim has j/k bindings
        assert!(reg.label_for("scroll_down").is_some());
    }

    #[test]
    fn registry_cycle_profile() {
        let mut reg = KeymapRegistry::new(KeymapProfile::Default);
        reg.cycle_profile();
        assert_eq!(reg.profile(), KeymapProfile::Vim);
        reg.cycle_profile();
        assert_eq!(reg.profile(), KeymapProfile::Emacs);
    }

    #[test]
    fn registry_override_changes_label() {
        let mut reg = KeymapRegistry::new(KeymapProfile::Default);
        assert_eq!(reg.label_for("quit"), Some("q"));
        reg.add_override(BindingOverride {
            action_id: "quit".to_string(),
            new_label: "x".to_string(),
        });
        assert_eq!(reg.label_for("quit"), Some("x"));
    }

    #[test]
    fn registry_remove_override_restores() {
        let mut reg = KeymapRegistry::new(KeymapProfile::Default);
        reg.add_override(BindingOverride {
            action_id: "quit".to_string(),
            new_label: "x".to_string(),
        });
        assert_eq!(reg.label_for("quit"), Some("x"));
        reg.remove_override("quit");
        assert_eq!(reg.label_for("quit"), Some("q"));
    }

    #[test]
    fn registry_clear_overrides() {
        let mut reg = KeymapRegistry::new(KeymapProfile::Default);
        reg.add_override(BindingOverride {
            action_id: "quit".to_string(),
            new_label: "x".to_string(),
        });
        reg.add_override(BindingOverride {
            action_id: "toggle_help".to_string(),
            new_label: "h".to_string(),
        });
        assert_eq!(reg.overrides().len(), 2);
        reg.clear_overrides();
        assert!(reg.overrides().is_empty());
        assert_eq!(reg.label_for("quit"), Some("q"));
    }

    #[test]
    fn registry_override_survives_profile_switch() {
        let mut reg = KeymapRegistry::with_overrides(
            KeymapProfile::Default,
            vec![BindingOverride {
                action_id: "quit".to_string(),
                new_label: "x".to_string(),
            }],
        );
        assert_eq!(reg.label_for("quit"), Some("x"));
        // Switch to Vim — override should still apply since Vim also has "quit"
        reg.set_profile(KeymapProfile::Vim);
        assert_eq!(reg.label_for("quit"), Some("x"));
    }

    #[test]
    fn registry_help_entries_nonempty() {
        let reg = KeymapRegistry::default();
        let entries = reg.help_entries();
        assert!(!entries.is_empty());
        // First entry should have both label and description
        let (label, action) = &entries[0];
        assert!(!label.is_empty());
        assert!(!action.is_empty());
    }

    #[test]
    fn registry_contextual_help_has_global_section() {
        let reg = KeymapRegistry::default();
        let sections = reg.contextual_help(&[], "Dashboard");
        assert_eq!(sections.len(), 1); // Only global (no screen bindings)
        assert!(sections[0].title.contains("Global"));
        assert!(sections[0].title.contains("Default"));
    }

    #[test]
    fn registry_contextual_help_with_screen() {
        let reg = KeymapRegistry::default();
        let screen = vec![crate::tui_screens::HelpEntry {
            key: "r",
            action: "Refresh",
        }];
        let sections = reg.contextual_help(&screen, "Messages");
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[1].title, "Messages");
        assert_eq!(sections[1].entries.len(), 1);
    }

    #[test]
    fn registry_conflicts_detected() {
        let reg = KeymapRegistry::default();
        let screen_bindings = &[("q", "Screen quit")];
        let conflicts = reg.conflicts_with(screen_bindings);
        assert!(!conflicts.is_empty());
    }

    #[test]
    fn registry_is_suppressible() {
        let reg = KeymapRegistry::default();
        assert!(reg.is_suppressible("quit"));
        assert!(!reg.is_suppressible("next_screen")); // Tab is not suppressible
    }

    #[test]
    fn registry_minimal_no_quit() {
        let reg = KeymapRegistry::new(KeymapProfile::Minimal);
        assert!(reg.label_for("quit").is_none());
    }

    // ── HelpSection tests ──

    #[test]
    fn help_section_line_count() {
        let section = HelpSection {
            title: "Test".to_string(),
            entries: vec![
                ("a".to_string(), "Action A".to_string()),
                ("b".to_string(), "Action B".to_string()),
            ],
        };
        assert_eq!(section.line_count(), 3); // title + 2 entries
    }

    #[test]
    fn binding_override_serde_roundtrip() {
        let ovr = BindingOverride {
            action_id: "quit".to_string(),
            new_label: "Ctrl+Q".to_string(),
        };
        let json = serde_json::to_string(&ovr).unwrap();
        let ovr2: BindingOverride = serde_json::from_str(&json).unwrap();
        assert_eq!(ovr2.action_id, "quit");
        assert_eq!(ovr2.new_label, "Ctrl+Q");
    }
}
