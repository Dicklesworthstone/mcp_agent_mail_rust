//! Auto-persistence for TUI layout preferences.
//!
//! Saves dock position, ratio, and visibility to the user's envfile
//! (`~/.config/mcp-agent-mail/config.env`) using the existing
//! [`mcp_agent_mail_core::config::update_envfile`] mechanism.
//!
//! # Debouncing
//!
//! Layout changes during drag or rapid key-presses are debounced so
//! the envfile is written at most once per [`SAVE_DEBOUNCE`] interval.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::config::update_envfile;

use serde::{Deserialize, Serialize};

use crate::tui_layout::{DockLayout, DockPosition};

/// Minimum interval between successive envfile writes.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

// ──────────────────────────────────────────────────────────────────────
// TuiPreferences — the saved state
// ──────────────────────────────────────────────────────────────────────

/// Accessibility settings for low-capability terminals and keyboard-only operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessibilitySettings {
    /// Use high-contrast color palette (brighter FG, darker BG).
    pub high_contrast: bool,
    /// Show context-sensitive key hints in the status line.
    pub key_hints: bool,
}

impl Default for AccessibilitySettings {
    fn default() -> Self {
        Self {
            high_contrast: false,
            key_hints: true,
        }
    }
}

/// Preferences that are persisted between TUI sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TuiPreferences {
    pub dock: DockLayout,
    #[serde(default)]
    pub accessibility: AccessibilitySettings,
}

impl TuiPreferences {
    /// Load preferences from the application config (which reads envfile).
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let position = match config.tui_dock_position.as_str() {
            "bottom" => DockPosition::Bottom,
            "top" => DockPosition::Top,
            "left" => DockPosition::Left,
            _ => DockPosition::Right,
        };
        let ratio = f32::from(config.tui_dock_ratio_percent.clamp(20, 80)) / 100.0;
        let visible = config.tui_dock_visible;

        Self {
            dock: DockLayout::new(position, ratio).with_visible(visible),
            accessibility: AccessibilitySettings {
                high_contrast: config.tui_high_contrast,
                key_hints: config.tui_key_hints,
            },
        }
    }

    /// Reset all preferences to their defaults.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Serialize preferences to a pretty-printed JSON string for export.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails (should not happen for this type).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserialize preferences from a JSON string (import).
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON is invalid or missing required fields.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Build the envfile key-value pairs for persistence.
    fn to_env_map(&self) -> HashMap<&'static str, String> {
        let mut map = HashMap::new();
        map.insert(
            "TUI_DOCK_POSITION",
            self.dock.position.label().to_ascii_lowercase(),
        );
        map.insert(
            "TUI_DOCK_RATIO_PERCENT",
            self.dock.ratio_percent().to_string(),
        );
        map.insert(
            "TUI_DOCK_VISIBLE",
            if self.dock.visible {
                "true".to_string()
            } else {
                "false".to_string()
            },
        );
        map.insert(
            "TUI_HIGH_CONTRAST",
            self.accessibility.high_contrast.to_string(),
        );
        map.insert("TUI_KEY_HINTS", self.accessibility.key_hints.to_string());
        map
    }
}

// ──────────────────────────────────────────────────────────────────────
// PreferencePersister — debounced writer
// ──────────────────────────────────────────────────────────────────────

/// Handles debounced persistence of TUI preferences to the envfile.
pub struct PreferencePersister {
    /// Path to the envfile (from `Config::console_persist_path`).
    path: PathBuf,
    /// Whether auto-save is enabled.
    auto_save: bool,
    /// Last saved snapshot (to skip redundant writes).
    last_saved: Option<TuiPreferences>,
    /// When a save was last requested (for debouncing).
    dirty_since: Option<Instant>,
    /// When we last wrote to disk.
    last_write: Instant,
}

impl PreferencePersister {
    /// Create a new persister from the application config.
    #[must_use]
    pub fn new(config: &Config) -> Self {
        Self {
            path: config.console_persist_path.clone(),
            auto_save: config.console_auto_save,
            last_saved: None,
            dirty_since: None,
            // Allow immediate first save.
            last_write: Instant::now()
                .checked_sub(SAVE_DEBOUNCE)
                .unwrap_or_else(Instant::now),
        }
    }

    /// Mark the current preferences as dirty (changed).
    ///
    /// The actual write happens on the next [`flush_if_due`] call
    /// after the debounce interval.
    pub fn mark_dirty(&mut self) {
        if self.dirty_since.is_none() {
            self.dirty_since = Some(Instant::now());
        }
    }

    /// Check if a debounced save is due and write if so.
    ///
    /// Returns `true` if a write was performed.
    pub fn flush_if_due(&mut self, prefs: &TuiPreferences) -> bool {
        if !self.auto_save || self.dirty_since.is_none() {
            return false;
        }
        if self.last_write.elapsed() < SAVE_DEBOUNCE {
            return false;
        }
        // Skip if nothing changed since last save.
        if self.last_saved.as_ref() == Some(prefs) {
            self.dirty_since = None;
            return false;
        }
        self.write_now(prefs)
    }

    /// Force an immediate save (e.g. on "Save Now" palette action).
    pub fn save_now(&mut self, prefs: &TuiPreferences) -> bool {
        self.write_now(prefs)
    }

    /// Write preferences to disk.
    fn write_now(&mut self, prefs: &TuiPreferences) -> bool {
        let map = prefs.to_env_map();
        match update_envfile(&self.path, &map) {
            Ok(()) => {
                self.last_saved = Some(prefs.clone());
                self.dirty_since = None;
                self.last_write = Instant::now();
                true
            }
            Err(e) => {
                // Log but don't crash — persistence is best-effort.
                eprintln!("tui_persist: failed to save preferences: {e}");
                false
            }
        }
    }

    /// Path to the envfile.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether auto-save is enabled.
    #[must_use]
    pub const fn auto_save(&self) -> bool {
        self.auto_save
    }

    /// Reset preferences to defaults and save immediately.
    ///
    /// Returns the default preferences if the save succeeded, `None` otherwise.
    pub fn reset_and_save(&mut self) -> Option<TuiPreferences> {
        let defaults = TuiPreferences::default();
        if self.write_now(&defaults) {
            Some(defaults)
        } else {
            None
        }
    }

    /// Export current preferences as JSON to a file next to the envfile.
    ///
    /// The export path is `<envfile_dir>/layout.json`.
    /// Returns the path on success.
    pub fn export_json(&self, prefs: &TuiPreferences) -> Result<PathBuf, std::io::Error> {
        let export_path = self.export_path();
        let json = prefs
            .to_json()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = export_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&export_path, json)?;
        Ok(export_path)
    }

    /// Import preferences from the JSON export file.
    ///
    /// Returns the imported preferences on success.
    pub fn import_json(&self) -> Result<TuiPreferences, std::io::Error> {
        let export_path = self.export_path();
        let json = std::fs::read_to_string(&export_path)?;
        TuiPreferences::from_json(&json)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// The path used for JSON import/export (`layout.json` next to envfile).
    #[must_use]
    pub fn export_path(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("layout.json")
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_defaults() {
        let config = Config::default();
        let prefs = TuiPreferences::from_config(&config);
        assert_eq!(prefs.dock.position, DockPosition::Right);
        assert!((prefs.dock.ratio - 0.4).abs() < f32::EPSILON);
        assert!(prefs.dock.visible);
    }

    #[test]
    fn from_config_custom() {
        let config = Config {
            tui_dock_position: "bottom".to_string(),
            tui_dock_ratio_percent: 50,
            tui_dock_visible: false,
            ..Config::default()
        };
        let prefs = TuiPreferences::from_config(&config);
        assert_eq!(prefs.dock.position, DockPosition::Bottom);
        assert!((prefs.dock.ratio - 0.5).abs() < f32::EPSILON);
        assert!(!prefs.dock.visible);
    }

    #[test]
    fn from_config_clamps_ratio() {
        let config_low = Config {
            tui_dock_ratio_percent: 5, // Below min (20)
            ..Config::default()
        };
        let prefs = TuiPreferences::from_config(&config_low);
        assert!((prefs.dock.ratio - 0.2).abs() < f32::EPSILON);

        let config_high = Config {
            tui_dock_ratio_percent: 99, // Above max (80)
            ..Config::default()
        };
        let prefs = TuiPreferences::from_config(&config_high);
        assert!((prefs.dock.ratio - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn from_config_invalid_position_defaults_to_right() {
        let config = Config {
            tui_dock_position: "diagonal".to_string(),
            ..Config::default()
        };
        let prefs = TuiPreferences::from_config(&config);
        assert_eq!(prefs.dock.position, DockPosition::Right);
    }

    #[test]
    fn env_map_roundtrip() {
        let prefs = TuiPreferences {
            dock: DockLayout::new(DockPosition::Left, 0.33).with_visible(false),
            ..Default::default()
        };
        let map = prefs.to_env_map();
        assert_eq!(map.get("TUI_DOCK_POSITION").unwrap(), "left");
        assert_eq!(map.get("TUI_DOCK_RATIO_PERCENT").unwrap(), "33");
        assert_eq!(map.get("TUI_DOCK_VISIBLE").unwrap(), "false");
    }

    #[test]
    fn persister_marks_dirty() {
        let config = Config::default();
        let mut persister = PreferencePersister::new(&config);
        assert!(persister.dirty_since.is_none());
        persister.mark_dirty();
        assert!(persister.dirty_since.is_some());
    }

    #[test]
    fn persister_skip_when_auto_save_disabled() {
        let config = Config {
            console_auto_save: false,
            ..Config::default()
        };
        let mut persister = PreferencePersister::new(&config);
        persister.mark_dirty();
        let prefs = TuiPreferences::from_config(&config);
        assert!(!persister.flush_if_due(&prefs));
    }

    #[test]
    fn persister_skip_when_not_dirty() {
        let config = Config::default();
        let mut persister = PreferencePersister::new(&config);
        let prefs = TuiPreferences::from_config(&config);
        assert!(!persister.flush_if_due(&prefs));
    }

    #[test]
    fn persister_writes_to_tmpfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_config.env");
        let config = Config {
            console_persist_path: path.clone(),
            console_auto_save: true,
            ..Config::default()
        };

        let prefs = TuiPreferences {
            dock: DockLayout::new(DockPosition::Bottom, 0.5).with_visible(true),
            ..Default::default()
        };

        let mut persister = PreferencePersister::new(&config);
        assert!(persister.save_now(&prefs));

        // Read back the file.
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("TUI_DOCK_POSITION=bottom"));
        assert!(contents.contains("TUI_DOCK_RATIO_PERCENT=50"));
        assert!(contents.contains("TUI_DOCK_VISIBLE=true"));
    }

    #[test]
    fn persister_skips_duplicate_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_config.env");
        let config = Config {
            console_persist_path: path,
            console_auto_save: true,
            ..Config::default()
        };

        let prefs = TuiPreferences::from_config(&config);
        let mut persister = PreferencePersister::new(&config);

        // First save should write.
        assert!(persister.save_now(&prefs));
        // Second save with same prefs — mark dirty, but flush should skip.
        persister.mark_dirty();
        // Need to wait for debounce or force.
        assert!(!persister.flush_if_due(&prefs)); // Same prefs = skip
    }

    // ── Default / reset ────────────────────────────────────────────

    #[test]
    fn default_prefs_matches_dock_default() {
        let prefs = TuiPreferences::default();
        assert_eq!(prefs.dock, DockLayout::default());
        assert_eq!(prefs.dock.position, DockPosition::Right);
        assert!((prefs.dock.ratio - 0.4).abs() < f32::EPSILON);
        assert!(prefs.dock.visible);
    }

    #[test]
    fn reset_restores_defaults() {
        let mut prefs = TuiPreferences {
            dock: DockLayout::new(DockPosition::Left, 0.6).with_visible(false),
            ..Default::default()
        };
        prefs.reset();
        assert_eq!(prefs, TuiPreferences::default());
    }

    // ── JSON round-trip ────────────────────────────────────────────

    #[test]
    fn json_roundtrip() {
        let prefs = TuiPreferences {
            dock: DockLayout::new(DockPosition::Bottom, 0.33).with_visible(false),
            ..Default::default()
        };
        let json = prefs.to_json().unwrap();
        let round = TuiPreferences::from_json(&json).unwrap();
        assert_eq!(round, prefs);
    }

    #[test]
    fn json_default_roundtrip() {
        let prefs = TuiPreferences::default();
        let json = prefs.to_json().unwrap();
        let round = TuiPreferences::from_json(&json).unwrap();
        assert_eq!(round, prefs);
    }

    #[test]
    fn json_is_pretty_printed() {
        let prefs = TuiPreferences::default();
        let json = prefs.to_json().unwrap();
        assert!(json.contains('\n'), "expected pretty-printed JSON");
    }

    #[test]
    fn from_json_rejects_invalid() {
        assert!(TuiPreferences::from_json("not json").is_err());
        assert!(TuiPreferences::from_json("{}").is_err()); // missing `dock`
        assert!(TuiPreferences::from_json(r#"{"dock": 42}"#).is_err());
    }

    #[test]
    fn from_json_accepts_all_positions() {
        for pos in ["bottom", "top", "left", "right"] {
            let json = format!(r#"{{"dock":{{"position":"{pos}","ratio":0.35,"visible":true}}}}"#);
            let prefs = TuiPreferences::from_json(&json).unwrap();
            assert!((prefs.dock.ratio - 0.35).abs() < f32::EPSILON);
        }
    }

    // ── Export / import file ───────────────────────────────────────

    #[test]
    fn export_path_is_layout_json() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            console_persist_path: dir.path().join("config.env"),
            ..Config::default()
        };
        let persister = PreferencePersister::new(&config);
        assert_eq!(persister.export_path(), dir.path().join("layout.json"));
    }

    #[test]
    fn export_import_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            console_persist_path: dir.path().join("config.env"),
            console_auto_save: true,
            ..Config::default()
        };

        let prefs = TuiPreferences {
            dock: DockLayout::new(DockPosition::Left, 0.55).with_visible(false),
            ..Default::default()
        };

        let persister = PreferencePersister::new(&config);
        let path = persister.export_json(&prefs).unwrap();
        assert!(path.exists());

        let imported = persister.import_json().unwrap();
        assert_eq!(imported, prefs);
    }

    #[test]
    fn import_fails_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            console_persist_path: dir.path().join("config.env"),
            ..Config::default()
        };
        let persister = PreferencePersister::new(&config);
        assert!(persister.import_json().is_err());
    }

    #[test]
    fn export_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let deep_path = dir.path().join("a").join("b").join("config.env");
        let config = Config {
            console_persist_path: deep_path,
            console_auto_save: true,
            ..Config::default()
        };

        let persister = PreferencePersister::new(&config);
        let prefs = TuiPreferences::default();
        let path = persister.export_json(&prefs).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn reset_and_save_writes_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.env");
        let config = Config {
            console_persist_path: path.clone(),
            console_auto_save: true,
            ..Config::default()
        };

        let mut persister = PreferencePersister::new(&config);
        let result = persister.reset_and_save();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), TuiPreferences::default());

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("TUI_DOCK_POSITION=right"));
        assert!(contents.contains("TUI_DOCK_RATIO_PERCENT=40"));
        assert!(contents.contains("TUI_DOCK_VISIBLE=true"));
    }

    // ── Interactive mutation sequences with persistence ────────────

    #[test]
    fn mutation_sequence_grow_shrink_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.env");
        let config = Config {
            console_persist_path: path.clone(),
            console_auto_save: true,
            ..Config::default()
        };

        let mut prefs = TuiPreferences::from_config(&config);
        let initial_ratio = prefs.dock.ratio;

        // Simulate interactive grow → grow → shrink sequence
        prefs.dock.grow_dock();
        prefs.dock.grow_dock();
        prefs.dock.shrink_dock();
        assert!(prefs.dock.ratio > initial_ratio);

        // Persist and verify round-trip
        let mut persister = PreferencePersister::new(&config);
        assert!(persister.save_now(&prefs));

        let contents = std::fs::read_to_string(&path).unwrap();
        let expected_pct = prefs.dock.ratio_percent().to_string();
        assert!(contents.contains(&format!("TUI_DOCK_RATIO_PERCENT={expected_pct}")));
    }

    #[test]
    fn mutation_sequence_cycle_position_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.env");
        let config = Config {
            console_persist_path: path.clone(),
            console_auto_save: true,
            ..Config::default()
        };

        let mut prefs = TuiPreferences::from_config(&config);
        assert_eq!(prefs.dock.position, DockPosition::Right);

        // Cycle: Right → Top → Left
        prefs.dock.cycle_position();
        assert_eq!(prefs.dock.position, DockPosition::Top);
        prefs.dock.cycle_position();
        assert_eq!(prefs.dock.position, DockPosition::Left);

        let mut persister = PreferencePersister::new(&config);
        assert!(persister.save_now(&prefs));

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("TUI_DOCK_POSITION=left"));
    }

    #[test]
    fn mutation_toggle_visible_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.env");
        let config = Config {
            console_persist_path: path.clone(),
            console_auto_save: true,
            ..Config::default()
        };

        let mut prefs = TuiPreferences::from_config(&config);
        assert!(prefs.dock.visible);

        prefs.dock.toggle_visible();
        assert!(!prefs.dock.visible);

        let mut persister = PreferencePersister::new(&config);
        assert!(persister.save_now(&prefs));

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("TUI_DOCK_VISIBLE=false"));
    }

    #[test]
    fn mutation_preset_application_persists() {
        use crate::tui_layout::DockPreset;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.env");
        let config = Config {
            console_persist_path: path.clone(),
            console_auto_save: true,
            ..Config::default()
        };

        let mut prefs = TuiPreferences::from_config(&config);
        prefs.dock.apply_preset(DockPreset::Wide);

        let mut persister = PreferencePersister::new(&config);
        assert!(persister.save_now(&prefs));

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("TUI_DOCK_RATIO_PERCENT=60"));
    }

    // ── Restore behavior edge cases ───────────────────────────────

    #[test]
    fn restore_from_config_boundary_ratios() {
        // Exactly at boundaries
        let config = Config {
            tui_dock_ratio_percent: 20,
            ..Config::default()
        };
        let prefs = TuiPreferences::from_config(&config);
        assert!((prefs.dock.ratio - 0.2).abs() < f32::EPSILON);

        let config = Config {
            tui_dock_ratio_percent: 80,
            ..Config::default()
        };
        let prefs = TuiPreferences::from_config(&config);
        assert!((prefs.dock.ratio - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn restore_from_config_all_positions() {
        for (name, expected) in [
            ("bottom", DockPosition::Bottom),
            ("top", DockPosition::Top),
            ("left", DockPosition::Left),
            ("right", DockPosition::Right),
        ] {
            let config = Config {
                tui_dock_position: name.to_string(),
                ..Config::default()
            };
            let prefs = TuiPreferences::from_config(&config);
            assert_eq!(prefs.dock.position, expected, "position={name}");
        }
    }

    #[test]
    fn restore_from_config_zero_ratio_clamps() {
        let config = Config {
            tui_dock_ratio_percent: 0,
            ..Config::default()
        };
        let prefs = TuiPreferences::from_config(&config);
        assert!((prefs.dock.ratio - 0.2).abs() < f32::EPSILON);
    }

    #[test]
    fn restore_from_config_100_ratio_clamps() {
        let config = Config {
            tui_dock_ratio_percent: 100,
            ..Config::default()
        };
        let prefs = TuiPreferences::from_config(&config);
        assert!((prefs.dock.ratio - 0.8).abs() < f32::EPSILON);
    }

    // ── Schema migration / malformed config handling ──────────────

    #[test]
    fn json_extra_fields_are_ignored() {
        let json =
            r#"{"dock":{"position":"left","ratio":0.3,"visible":true},"extra_field":"ignored"}"#;
        let prefs = TuiPreferences::from_json(json).unwrap();
        assert_eq!(prefs.dock.position, DockPosition::Left);
    }

    #[test]
    fn json_missing_visible_field_fails() {
        let json = r#"{"dock":{"position":"left","ratio":0.3}}"#;
        // visible is required (no default)
        assert!(TuiPreferences::from_json(json).is_err());
    }

    #[test]
    fn json_invalid_position_value_fails() {
        let json = r#"{"dock":{"position":"diagonal","ratio":0.3,"visible":true}}"#;
        assert!(TuiPreferences::from_json(json).is_err());
    }

    #[test]
    fn json_ratio_out_of_range_is_preserved() {
        // JSON round-trip preserves the exact ratio (DockLayout::new would clamp)
        let json = r#"{"dock":{"position":"right","ratio":0.95,"visible":true}}"#;
        let prefs = TuiPreferences::from_json(json).unwrap();
        // Serde doesn't call DockLayout::new, so the raw value is stored
        assert!((prefs.dock.ratio - 0.95).abs() < f32::EPSILON);
    }

    #[test]
    fn json_negative_ratio_is_preserved() {
        let json = r#"{"dock":{"position":"right","ratio":-0.1,"visible":true}}"#;
        let prefs = TuiPreferences::from_json(json).unwrap();
        // Serde doesn't validate range
        assert!(prefs.dock.ratio < 0.0);
    }

    #[test]
    fn env_map_full_cycle_with_mutations() {
        // Start with defaults, mutate, persist via env map, restore via config
        let mut prefs = TuiPreferences::default();
        prefs.dock.cycle_position(); // Right → Top
        prefs.dock.grow_dock(); // 40% → 45%
        prefs.dock.toggle_visible(); // true → false

        let map = prefs.to_env_map();
        assert_eq!(map["TUI_DOCK_POSITION"], "top");
        assert_eq!(map["TUI_DOCK_RATIO_PERCENT"], "45");
        assert_eq!(map["TUI_DOCK_VISIBLE"], "false");

        // Simulate restore via config with the same values
        let config = Config {
            tui_dock_position: map["TUI_DOCK_POSITION"].clone(),
            tui_dock_ratio_percent: map["TUI_DOCK_RATIO_PERCENT"].parse::<u16>().unwrap(),
            tui_dock_visible: map["TUI_DOCK_VISIBLE"] == "true",
            ..Config::default()
        };
        let restored = TuiPreferences::from_config(&config);
        assert_eq!(restored.dock.position, prefs.dock.position);
        assert!((restored.dock.ratio - prefs.dock.ratio).abs() < f32::EPSILON);
        assert_eq!(restored.dock.visible, prefs.dock.visible);
    }

    #[test]
    fn export_overwrite_with_different_prefs() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            console_persist_path: dir.path().join("config.env"),
            console_auto_save: true,
            ..Config::default()
        };
        let persister = PreferencePersister::new(&config);

        // Export first layout
        let prefs1 = TuiPreferences {
            dock: DockLayout::new(DockPosition::Left, 0.3),
            ..Default::default()
        };
        persister.export_json(&prefs1).unwrap();

        // Overwrite with different layout
        let prefs2 = TuiPreferences {
            dock: DockLayout::new(DockPosition::Top, 0.6).with_visible(false),
            ..Default::default()
        };
        persister.export_json(&prefs2).unwrap();

        // Import should get the second layout
        let imported = persister.import_json().unwrap();
        assert_eq!(imported.dock.position, DockPosition::Top);
        assert!(!imported.dock.visible);
    }

    #[test]
    fn import_corrupt_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            console_persist_path: dir.path().join("config.env"),
            ..Config::default()
        };
        let persister = PreferencePersister::new(&config);

        // Write garbage to the export path
        std::fs::write(persister.export_path(), "not valid json!").unwrap();
        assert!(persister.import_json().is_err());
    }

    #[test]
    fn persist_and_restore_full_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.env");

        // Session 1: Start with custom config, mutate, save
        let config1 = Config {
            console_persist_path: path.clone(),
            console_auto_save: true,
            tui_dock_position: "bottom".to_string(),
            tui_dock_ratio_percent: 33,
            tui_dock_visible: true,
            ..Config::default()
        };
        let mut prefs1 = TuiPreferences::from_config(&config1);
        prefs1.dock.grow_dock(); // 33% → 38%
        prefs1.dock.cycle_position(); // Bottom → Right

        let mut persister = PreferencePersister::new(&config1);
        assert!(persister.save_now(&prefs1));

        // Session 2: Read the envfile, verify the values survived
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("TUI_DOCK_POSITION=right"));
        assert!(contents.contains("TUI_DOCK_RATIO_PERCENT=38"));
        assert!(contents.contains("TUI_DOCK_VISIBLE=true"));
    }

    // ── AccessibilitySettings tests ─────────────────────────────

    #[test]
    fn accessibility_default() {
        let settings = AccessibilitySettings::default();
        assert!(!settings.high_contrast);
        assert!(settings.key_hints);
    }

    #[test]
    fn accessibility_from_config() {
        let config = Config {
            tui_high_contrast: true,
            tui_key_hints: false,
            ..Config::default()
        };
        let prefs = TuiPreferences::from_config(&config);
        assert!(prefs.accessibility.high_contrast);
        assert!(!prefs.accessibility.key_hints);
    }

    #[test]
    fn accessibility_json_roundtrip() {
        let prefs = TuiPreferences {
            dock: DockLayout::default(),
            accessibility: AccessibilitySettings {
                high_contrast: true,
                key_hints: false,
            },
        };
        let json = prefs.to_json().unwrap();
        let restored = TuiPreferences::from_json(&json).unwrap();
        assert_eq!(prefs, restored);
    }

    #[test]
    fn accessibility_json_missing_field_uses_default() {
        // Old JSON without accessibility section should deserialize with defaults.
        let json = r#"{"dock":{"position":"right","ratio":0.4,"visible":true}}"#;
        let prefs = TuiPreferences::from_json(json).unwrap();
        assert!(!prefs.accessibility.high_contrast);
        assert!(prefs.accessibility.key_hints);
    }

    #[test]
    fn accessibility_persisted_to_env_map() {
        let prefs = TuiPreferences {
            dock: DockLayout::default(),
            accessibility: AccessibilitySettings {
                high_contrast: true,
                key_hints: false,
            },
        };
        let map = prefs.to_env_map();
        assert_eq!(map.get("TUI_HIGH_CONTRAST").unwrap(), "true");
        assert_eq!(map.get("TUI_KEY_HINTS").unwrap(), "false");
    }

    #[test]
    fn accessibility_reset_restores_defaults() {
        let mut prefs = TuiPreferences {
            dock: DockLayout::default(),
            accessibility: AccessibilitySettings {
                high_contrast: true,
                key_hints: false,
            },
        };
        prefs.reset();
        assert!(!prefs.accessibility.high_contrast);
        assert!(prefs.accessibility.key_hints);
    }
}
