//! Dashboard preset manager for customizable layouts and widget configurations (br-3vwi.6.4).
//!
//! Operators can tailor the dashboard for different workflows (incident triage,
//! backlog review, ack chasing, contention diagnosis). Presets are persisted as
//! versioned JSON artifacts following the [`MacroEngine`](crate::tui_macro) pattern.
//!
//! # Architecture
//!
//! - [`DashboardPreset`]: serializable layout + widget configuration
//! - [`PresetManager`]: CRUD operations with directory-based JSON storage
//! - Built-in presets for common workflows (always available, not deletable)

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tui_layout::{
    DensityHint, DockLayout, PanelConstraint, PanelPolicy, PanelSlot, SplitAxis, TerminalClass,
};

// ── Schema version ──────────────────────────────────────────────────────

/// Current preset schema version for forward compatibility.
const SCHEMA_VERSION: u32 = 1;

// ── Widget slot configuration ───────────────────────────────────────────

/// Identifies a widget type that can be placed in a dashboard slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    /// `MetricTile` — single KPI with trend and optional sparkline.
    MetricTile,
    /// `ReservationGauge` — capacity/utilization bar.
    Gauge,
    /// `AnomalyCard` — severity-colored diagnostic card.
    AnomalyCard,
    /// `PercentileRibbon` — p50/p95/p99 latency bands.
    PercentileRibbon,
    /// `Leaderboard` — ranked list of top entities.
    Leaderboard,
    /// `HeatmapGrid` — 2D color-coded matrix.
    Heatmap,
    /// `BrailleActivity` — braille-dot sparkline.
    BrailleActivity,
    /// Event log (scrollable filtered list).
    EventLog,
    /// Agent heatmap (cross-agent activity matrix).
    AgentHeatmap,
}

/// Configuration for a single widget slot within a dashboard band.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetSlotConfig {
    /// What kind of widget to render.
    pub kind: WidgetKind,
    /// Human-readable label override (if `None`, uses widget default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Whether this slot is visible (allows hiding without removing).
    #[serde(default = "default_true")]
    pub visible: bool,
}

const fn default_true() -> bool {
    true
}

impl WidgetSlotConfig {
    /// Create a visible slot of the given kind.
    #[must_use]
    pub const fn new(kind: WidgetKind) -> Self {
        Self {
            kind,
            label: None,
            visible: true,
        }
    }

    /// Set a custom label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

// ── Band configuration ──────────────────────────────────────────────────

/// A horizontal band in the dashboard layout.
///
/// Bands are stacked vertically. Each band has a height budget (in rows)
/// per terminal class and contains an ordered list of widget slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BandConfig {
    /// Unique band identifier (e.g. "summary", "anomaly", "main", "footer").
    pub id: String,
    /// Height in terminal rows for each terminal class.
    /// Index by [`TerminalClass`] ordinal (Tiny=0, Compact=1, Normal=2, Wide=3, UltraWide=4).
    pub heights: [u16; 5],
    /// Widgets in this band, rendered left-to-right.
    pub widgets: Vec<WidgetSlotConfig>,
}

impl BandConfig {
    /// Create a band with uniform height and no widgets.
    #[must_use]
    pub fn new(id: impl Into<String>, height: u16) -> Self {
        Self {
            id: id.into(),
            heights: [height; 5],
            widgets: Vec::new(),
        }
    }

    /// Set height for a specific terminal class.
    #[must_use]
    pub const fn height_at(mut self, class: TerminalClass, h: u16) -> Self {
        self.heights[class as usize] = h;
        self
    }

    /// Add a widget slot.
    #[must_use]
    pub fn widget(mut self, slot: WidgetSlotConfig) -> Self {
        self.widgets.push(slot);
        self
    }

    /// Get the height for a given terminal class.
    #[must_use]
    pub const fn height_for(&self, class: TerminalClass) -> u16 {
        self.heights[class as usize]
    }
}

// ── Panel layout configuration ──────────────────────────────────────────

/// Serializable panel layout specification.
///
/// This mirrors `ReactiveLayout` but is fully serializable. At runtime,
/// the dashboard converts this to a `ReactiveLayout` for rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelLayoutConfig {
    /// Panel policies for the reactive layout engine.
    pub panels: Vec<PanelPolicy>,
}

impl PanelLayoutConfig {
    /// Default two-panel layout (primary + footer).
    #[must_use]
    pub fn default_two_panel() -> Self {
        Self {
            panels: vec![
                PanelPolicy::new(
                    PanelSlot::Primary,
                    0,
                    SplitAxis::Horizontal,
                    PanelConstraint::visible(1.0, 0),
                ),
                PanelPolicy::new(
                    PanelSlot::Footer,
                    2,
                    SplitAxis::Horizontal,
                    PanelConstraint::HIDDEN,
                )
                .at(TerminalClass::Compact, PanelConstraint::visible(0.0, 1))
                .at(TerminalClass::Normal, PanelConstraint::visible(0.0, 1))
                .at(TerminalClass::Wide, PanelConstraint::visible(0.0, 1))
                .at(TerminalClass::UltraWide, PanelConstraint::visible(0.0, 1)),
            ],
        }
    }

    /// Three-panel layout with inspector.
    #[must_use]
    pub fn with_inspector() -> Self {
        Self {
            panels: vec![
                PanelPolicy::new(
                    PanelSlot::Primary,
                    0,
                    SplitAxis::Horizontal,
                    PanelConstraint::visible(1.0, 0),
                )
                .at(TerminalClass::Normal, PanelConstraint::visible(0.65, 40))
                .at(TerminalClass::Wide, PanelConstraint::visible(0.65, 60))
                .at(TerminalClass::UltraWide, PanelConstraint::visible(0.60, 80)),
                PanelPolicy::new(
                    PanelSlot::Inspector,
                    1,
                    SplitAxis::Vertical,
                    PanelConstraint::HIDDEN,
                )
                .at(TerminalClass::Normal, PanelConstraint::visible(0.35, 30))
                .at(TerminalClass::Wide, PanelConstraint::visible(0.35, 40))
                .at(TerminalClass::UltraWide, PanelConstraint::visible(0.40, 50)),
                PanelPolicy::new(
                    PanelSlot::Footer,
                    2,
                    SplitAxis::Horizontal,
                    PanelConstraint::HIDDEN,
                )
                .at(TerminalClass::Compact, PanelConstraint::visible(0.0, 1))
                .at(TerminalClass::Normal, PanelConstraint::visible(0.0, 1))
                .at(TerminalClass::Wide, PanelConstraint::visible(0.0, 1))
                .at(TerminalClass::UltraWide, PanelConstraint::visible(0.0, 1)),
            ],
        }
    }
}

// ── Dashboard Preset ────────────────────────────────────────────────────

/// A named, versioned dashboard preset that captures layout and widget
/// configuration for a specific workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardPreset {
    /// Schema version for forward compatibility.
    pub version: u32,
    /// Unique preset name (used as key and filename slug).
    pub name: String,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether this is a built-in preset (not deletable/editable).
    #[serde(default)]
    pub builtin: bool,
    /// Dock layout override.
    #[serde(default)]
    pub dock: Option<DockLayout>,
    /// Density hint override (if `None`, auto-detect from terminal size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub density_override: Option<DensityHint>,
    /// Panel layout specification.
    pub panel_layout: PanelLayoutConfig,
    /// Dashboard bands (stacked vertically).
    pub bands: Vec<BandConfig>,
    /// Panel visibility overrides per slot.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub panel_visibility: HashMap<PanelSlot, bool>,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// ISO-8601 last-modified timestamp.
    pub updated_at: String,
}

impl DashboardPreset {
    /// Create a new preset with the given name, layout, and bands.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        panel_layout: PanelLayoutConfig,
        bands: Vec<BandConfig>,
    ) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            version: SCHEMA_VERSION,
            name: name.into(),
            description: None,
            builtin: false,
            dock: None,
            density_override: None,
            panel_layout,
            bands,
            panel_visibility: HashMap::new(),
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// Set description.
    #[must_use]
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Mark as built-in.
    #[must_use]
    pub const fn as_builtin(mut self) -> Self {
        self.builtin = true;
        self
    }

    /// Set dock layout override.
    #[must_use]
    pub const fn with_dock(mut self, dock: DockLayout) -> Self {
        self.dock = Some(dock);
        self
    }

    /// Set density override.
    #[must_use]
    pub const fn with_density(mut self, density: DensityHint) -> Self {
        self.density_override = Some(density);
        self
    }

    /// Set visibility for a specific panel slot.
    #[must_use]
    pub fn with_panel_visible(mut self, slot: PanelSlot, visible: bool) -> Self {
        self.panel_visibility.insert(slot, visible);
        self
    }

    /// Touch the `updated_at` timestamp.
    pub fn touch(&mut self) {
        self.updated_at = chrono::Utc::now().to_rfc3339();
    }

    /// Serialize to pretty JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserialize from JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON is invalid.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

// ── Built-in presets ────────────────────────────────────────────────────

/// Create the "default" built-in preset matching current hardcoded behavior.
#[must_use]
pub fn builtin_default() -> DashboardPreset {
    DashboardPreset::new(
        "default",
        PanelLayoutConfig::default_two_panel(),
        vec![
            BandConfig::new("summary", 3)
                .height_at(TerminalClass::Tiny, 1)
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile))
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile))
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile))
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile))
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile)),
            BandConfig::new("anomaly", 4)
                .height_at(TerminalClass::Tiny, 0)
                .height_at(TerminalClass::Compact, 0)
                .widget(WidgetSlotConfig::new(WidgetKind::AnomalyCard)),
            BandConfig::new("main", 0) // 0 = fill remaining
                .widget(WidgetSlotConfig::new(WidgetKind::EventLog)),
        ],
    )
    .with_description("Standard operator dashboard with KPIs, anomaly rail, and event log.")
    .as_builtin()
}

/// Create the "incident-triage" preset optimized for active incident response.
#[must_use]
pub fn builtin_incident_triage() -> DashboardPreset {
    DashboardPreset::new(
        "incident-triage",
        PanelLayoutConfig::with_inspector(),
        vec![
            BandConfig::new("summary", 3)
                .height_at(TerminalClass::Tiny, 1)
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile).with_label("Error Rate"))
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile).with_label("Avg Latency"))
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile).with_label("Requests")),
            BandConfig::new("anomaly", 6)
                .height_at(TerminalClass::Tiny, 0)
                .height_at(TerminalClass::Compact, 4)
                .widget(WidgetSlotConfig::new(WidgetKind::AnomalyCard))
                .widget(WidgetSlotConfig::new(WidgetKind::PercentileRibbon)),
            BandConfig::new("main", 0).widget(WidgetSlotConfig::new(WidgetKind::EventLog)),
        ],
    )
    .with_description(
        "Prioritizes error rates, latency anomalies, and event stream for rapid triage.",
    )
    .with_density(DensityHint::Normal)
    .as_builtin()
}

/// Create the "backlog-review" preset focused on ack chasing and queue depth.
#[must_use]
pub fn builtin_backlog_review() -> DashboardPreset {
    DashboardPreset::new(
        "backlog-review",
        PanelLayoutConfig::default_two_panel(),
        vec![
            BandConfig::new("summary", 3)
                .height_at(TerminalClass::Tiny, 1)
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile).with_label("Ack Pending"))
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile).with_label("Messages"))
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile).with_label("Agents"))
                .widget(WidgetSlotConfig::new(WidgetKind::Gauge).with_label("Event Ring")),
            BandConfig::new("main", 0)
                .widget(WidgetSlotConfig::new(WidgetKind::Leaderboard).with_label("Top Tools"))
                .widget(WidgetSlotConfig::new(WidgetKind::EventLog)),
        ],
    )
    .with_description("Focused on ack backlog, queue depths, and tool activity for backlog triage.")
    .as_builtin()
}

/// Create the "contention-diagnosis" preset for resource contention analysis.
#[must_use]
pub fn builtin_contention_diagnosis() -> DashboardPreset {
    DashboardPreset::new(
        "contention-diagnosis",
        PanelLayoutConfig::with_inspector(),
        vec![
            BandConfig::new("summary", 3)
                .height_at(TerminalClass::Tiny, 1)
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile).with_label("Reservations"))
                .widget(WidgetSlotConfig::new(WidgetKind::MetricTile).with_label("Contention"))
                .widget(WidgetSlotConfig::new(WidgetKind::Gauge).with_label("Event Ring")),
            BandConfig::new("heatmap", 8)
                .height_at(TerminalClass::Tiny, 0)
                .height_at(TerminalClass::Compact, 0)
                .widget(WidgetSlotConfig::new(WidgetKind::AgentHeatmap))
                .widget(WidgetSlotConfig::new(WidgetKind::Heatmap)),
            BandConfig::new("main", 0).widget(WidgetSlotConfig::new(WidgetKind::EventLog)),
        ],
    )
    .with_description(
        "Highlights reservation contention, agent activity heatmaps, and resource pressure.",
    )
    .with_density(DensityHint::Detailed)
    .as_builtin()
}

/// Returns all built-in presets.
#[must_use]
pub fn all_builtins() -> Vec<DashboardPreset> {
    vec![
        builtin_default(),
        builtin_incident_triage(),
        builtin_backlog_review(),
        builtin_contention_diagnosis(),
    ]
}

// ── Preset Manager ──────────────────────────────────────────────────────

/// Manages dashboard presets with directory-based JSON persistence.
///
/// Built-in presets are always available and cannot be deleted or overwritten.
/// User presets are stored as `<name>.json` files in the storage directory.
pub struct PresetManager {
    /// All loaded presets (built-in + user).
    presets: BTreeMap<String, DashboardPreset>,
    /// Directory for user preset files.
    storage_dir: PathBuf,
    /// Currently active preset name.
    active: String,
}

impl PresetManager {
    /// Create a new manager, loading built-in presets and any saved user presets.
    ///
    /// # Arguments
    ///
    /// * `storage_dir` - Directory for user preset JSON files.
    /// * `active_name` - Name of the initially active preset (falls back to "default").
    #[must_use]
    pub fn new(storage_dir: PathBuf, active_name: Option<&str>) -> Self {
        let mut presets = BTreeMap::new();

        // Load built-ins first.
        for preset in all_builtins() {
            presets.insert(preset.name.clone(), preset);
        }

        // Load user presets from disk (non-fatal on errors).
        if let Ok(entries) = std::fs::read_dir(&storage_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    if let Ok(data) = std::fs::read_to_string(&path) {
                        if let Ok(preset) = DashboardPreset::from_json(&data) {
                            // Don't overwrite built-ins with user files.
                            if !presets.get(&preset.name).is_some_and(|p| p.builtin) {
                                presets.insert(preset.name.clone(), preset);
                            }
                        }
                    }
                }
            }
        }

        let active = active_name
            .filter(|n| presets.contains_key(*n))
            .unwrap_or("default")
            .to_string();

        Self {
            presets,
            storage_dir,
            active,
        }
    }

    /// List all preset names in sorted order.
    #[must_use]
    pub fn list(&self) -> Vec<&str> {
        self.presets.keys().map(String::as_str).collect()
    }

    /// Get a preset by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&DashboardPreset> {
        self.presets.get(name)
    }

    /// Get the currently active preset.
    #[must_use]
    pub fn active(&self) -> &DashboardPreset {
        self.presets.get(&self.active).unwrap_or_else(|| {
            self.presets
                .get("default")
                .expect("default preset must exist")
        })
    }

    /// Get the active preset name.
    #[must_use]
    pub fn active_name(&self) -> &str {
        &self.active
    }

    /// Activate a preset by name. Returns `false` if the preset doesn't exist.
    pub fn activate(&mut self, name: &str) -> bool {
        if self.presets.contains_key(name) {
            self.active = name.to_string();
            true
        } else {
            false
        }
    }

    /// Cycle to the next preset (alphabetical order).
    pub fn cycle_next(&mut self) {
        let names: Vec<&String> = self.presets.keys().collect();
        if names.is_empty() {
            return;
        }
        let current_idx = names.iter().position(|n| **n == self.active).unwrap_or(0);
        let next_idx = (current_idx + 1) % names.len();
        self.active = names[next_idx].clone();
    }

    /// Cycle to the previous preset.
    pub fn cycle_prev(&mut self) {
        let names: Vec<&String> = self.presets.keys().collect();
        if names.is_empty() {
            return;
        }
        let current_idx = names.iter().position(|n| **n == self.active).unwrap_or(0);
        let prev_idx = if current_idx == 0 {
            names.len() - 1
        } else {
            current_idx - 1
        };
        self.active = names[prev_idx].clone();
    }

    /// Save a user preset to disk. Returns an error if it's a built-in name.
    ///
    /// # Errors
    ///
    /// Returns an error if the preset is built-in or disk write fails.
    pub fn save(&mut self, preset: DashboardPreset) -> Result<(), PresetError> {
        if self.presets.get(&preset.name).is_some_and(|p| p.builtin) {
            return Err(PresetError::BuiltinReadOnly(preset.name));
        }

        let path = self.preset_path(&preset.name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(PresetError::Io)?;
        }

        let json = preset.to_json().map_err(PresetError::Serialize)?;
        std::fs::write(&path, json).map_err(PresetError::Io)?;

        self.presets.insert(preset.name.clone(), preset);
        Ok(())
    }

    /// Delete a user preset. Returns an error if it's a built-in.
    ///
    /// # Errors
    ///
    /// Returns an error if the preset is built-in or disk delete fails.
    pub fn delete(&mut self, name: &str) -> Result<(), PresetError> {
        if self.presets.get(name).is_some_and(|p| p.builtin) {
            return Err(PresetError::BuiltinReadOnly(name.to_string()));
        }

        if self.presets.remove(name).is_none() {
            return Err(PresetError::NotFound(name.to_string()));
        }

        // If the active preset was deleted, fall back to default.
        if self.active == name {
            self.active = "default".to_string();
        }

        let path = self.preset_path(name);
        if path.exists() {
            std::fs::remove_file(&path).map_err(PresetError::Io)?;
        }

        Ok(())
    }

    /// Number of presets (built-in + user).
    #[must_use]
    pub fn count(&self) -> usize {
        self.presets.len()
    }

    /// Number of user (non-builtin) presets.
    #[must_use]
    pub fn user_count(&self) -> usize {
        self.presets.values().filter(|p| !p.builtin).count()
    }

    /// Storage directory path.
    #[must_use]
    pub fn storage_dir(&self) -> &Path {
        &self.storage_dir
    }

    /// Compute the file path for a preset name.
    fn preset_path(&self, name: &str) -> PathBuf {
        let safe_name = sanitize_filename(name);
        self.storage_dir.join(format!("{safe_name}.json"))
    }
}

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors from preset operations.
#[derive(Debug)]
pub enum PresetError {
    /// Attempted to modify a built-in preset.
    BuiltinReadOnly(String),
    /// Preset not found.
    NotFound(String),
    /// I/O error during read/write.
    Io(std::io::Error),
    /// JSON serialization error.
    Serialize(serde_json::Error),
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BuiltinReadOnly(name) => write!(f, "preset '{name}' is built-in and read-only"),
            Self::NotFound(name) => write!(f, "preset '{name}' not found"),
            Self::Io(e) => write!(f, "preset I/O error: {e}"),
            Self::Serialize(e) => write!(f, "preset serialization error: {e}"),
        }
    }
}

impl std::error::Error for PresetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Serialize(e) => Some(e),
            _ => None,
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Sanitize a name for use as a filename (remove non-alphanumeric except hyphens).
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── WidgetSlotConfig ────────────────────────────────────────────

    #[test]
    fn widget_slot_default_visible() {
        let slot = WidgetSlotConfig::new(WidgetKind::MetricTile);
        assert!(slot.visible);
        assert!(slot.label.is_none());
    }

    #[test]
    fn widget_slot_with_label() {
        let slot = WidgetSlotConfig::new(WidgetKind::Gauge).with_label("CPU Usage");
        assert_eq!(slot.label.as_deref(), Some("CPU Usage"));
    }

    // ── BandConfig ──────────────────────────────────────────────────

    #[test]
    fn band_uniform_height() {
        let band = BandConfig::new("summary", 3);
        assert_eq!(band.height_for(TerminalClass::Tiny), 3);
        assert_eq!(band.height_for(TerminalClass::UltraWide), 3);
    }

    #[test]
    fn band_per_class_height() {
        let band = BandConfig::new("summary", 3)
            .height_at(TerminalClass::Tiny, 1)
            .height_at(TerminalClass::UltraWide, 5);
        assert_eq!(band.height_for(TerminalClass::Tiny), 1);
        assert_eq!(band.height_for(TerminalClass::Normal), 3);
        assert_eq!(band.height_for(TerminalClass::UltraWide), 5);
    }

    #[test]
    fn band_widget_builder() {
        let band = BandConfig::new("main", 0)
            .widget(WidgetSlotConfig::new(WidgetKind::EventLog))
            .widget(WidgetSlotConfig::new(WidgetKind::Leaderboard));
        assert_eq!(band.widgets.len(), 2);
    }

    // ── PanelLayoutConfig ───────────────────────────────────────────

    #[test]
    fn default_two_panel_has_primary_and_footer() {
        let layout = PanelLayoutConfig::default_two_panel();
        assert_eq!(layout.panels.len(), 2);
        assert_eq!(layout.panels[0].slot, PanelSlot::Primary);
        assert_eq!(layout.panels[1].slot, PanelSlot::Footer);
    }

    #[test]
    fn with_inspector_has_three_panels() {
        let layout = PanelLayoutConfig::with_inspector();
        assert_eq!(layout.panels.len(), 3);
        assert!(layout.panels.iter().any(|p| p.slot == PanelSlot::Inspector));
    }

    // ── DashboardPreset ─────────────────────────────────────────────

    #[test]
    fn preset_new_has_schema_version() {
        let preset = DashboardPreset::new("test", PanelLayoutConfig::default_two_panel(), vec![]);
        assert_eq!(preset.version, SCHEMA_VERSION);
        assert!(!preset.builtin);
        assert!(preset.dock.is_none());
        assert!(preset.density_override.is_none());
    }

    #[test]
    fn preset_builder_methods() {
        let preset = DashboardPreset::new("test", PanelLayoutConfig::default_two_panel(), vec![])
            .with_description("A test preset")
            .with_density(DensityHint::Compact)
            .with_panel_visible(PanelSlot::Inspector, false)
            .as_builtin();

        assert!(preset.builtin);
        assert_eq!(preset.description.as_deref(), Some("A test preset"));
        assert_eq!(preset.density_override, Some(DensityHint::Compact));
        assert_eq!(
            preset.panel_visibility.get(&PanelSlot::Inspector),
            Some(&false)
        );
    }

    #[test]
    fn preset_touch_updates_timestamp() {
        let mut preset =
            DashboardPreset::new("test", PanelLayoutConfig::default_two_panel(), vec![]);
        let original = preset.updated_at.clone();
        std::thread::sleep(std::time::Duration::from_millis(10));
        preset.touch();
        assert_ne!(preset.updated_at, original);
    }

    #[test]
    fn preset_json_roundtrip() {
        let preset = builtin_default();
        let json = preset.to_json().unwrap();
        let restored = DashboardPreset::from_json(&json).unwrap();
        assert_eq!(restored.name, "default");
        assert!(restored.builtin);
        assert_eq!(restored.bands.len(), preset.bands.len());
    }

    #[test]
    fn preset_json_pretty_printed() {
        let preset = builtin_default();
        let json = preset.to_json().unwrap();
        assert!(json.contains('\n'));
    }

    #[test]
    fn preset_json_invalid_fails() {
        assert!(DashboardPreset::from_json("not json").is_err());
        assert!(DashboardPreset::from_json("{}").is_err());
    }

    // ── Built-in presets ────────────────────────────────────────────

    #[test]
    fn all_builtins_count() {
        let builtins = all_builtins();
        assert_eq!(builtins.len(), 4);
        assert!(builtins.iter().all(|p| p.builtin));
    }

    #[test]
    fn builtin_names_are_unique() {
        let builtins = all_builtins();
        let mut names: Vec<&str> = builtins.iter().map(|p| p.name.as_str()).collect();
        let original_len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), original_len, "duplicate builtin preset names");
    }

    #[test]
    fn builtin_default_has_bands() {
        let preset = builtin_default();
        assert!(!preset.bands.is_empty());
        assert_eq!(preset.name, "default");
    }

    #[test]
    fn builtin_incident_triage_uses_inspector() {
        let preset = builtin_incident_triage();
        assert_eq!(preset.panel_layout.panels.len(), 3);
    }

    #[test]
    fn builtin_contention_diagnosis_has_heatmap_band() {
        let preset = builtin_contention_diagnosis();
        assert!(preset.bands.iter().any(|b| b.id == "heatmap"));
    }

    // ── PresetManager ───────────────────────────────────────────────

    #[test]
    fn manager_loads_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = PresetManager::new(dir.path().to_path_buf(), None);
        assert_eq!(mgr.count(), 4);
        assert_eq!(mgr.user_count(), 0);
        assert_eq!(mgr.active_name(), "default");
    }

    #[test]
    fn manager_activate() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PresetManager::new(dir.path().to_path_buf(), None);
        assert!(mgr.activate("incident-triage"));
        assert_eq!(mgr.active_name(), "incident-triage");
        assert!(!mgr.activate("nonexistent"));
        assert_eq!(mgr.active_name(), "incident-triage");
    }

    #[test]
    fn manager_initial_active_from_arg() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = PresetManager::new(dir.path().to_path_buf(), Some("backlog-review"));
        assert_eq!(mgr.active_name(), "backlog-review");
    }

    #[test]
    fn manager_initial_active_invalid_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = PresetManager::new(dir.path().to_path_buf(), Some("nonexistent"));
        assert_eq!(mgr.active_name(), "default");
    }

    #[test]
    fn manager_cycle_next() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PresetManager::new(dir.path().to_path_buf(), None);
        let initial = mgr.active_name().to_string();
        mgr.cycle_next();
        assert_ne!(mgr.active_name(), initial);
    }

    #[test]
    fn manager_cycle_wraps_around() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PresetManager::new(dir.path().to_path_buf(), None);
        let count = mgr.count();
        for _ in 0..count {
            mgr.cycle_next();
        }
        assert_eq!(mgr.active_name(), "default");
    }

    #[test]
    fn manager_cycle_prev() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PresetManager::new(dir.path().to_path_buf(), None);
        // Start at "default". Sorted order: backlog-review, contention-diagnosis, default, incident-triage.
        assert_eq!(mgr.active_name(), "default");
        mgr.cycle_prev();
        // prev from "default" (idx 2) goes to "contention-diagnosis" (idx 1).
        assert_eq!(mgr.active_name(), "contention-diagnosis");

        // Cycle prev to first, then wrap to last.
        mgr.cycle_prev(); // → "backlog-review"
        assert_eq!(mgr.active_name(), "backlog-review");
        mgr.cycle_prev(); // → "incident-triage" (wrap)
        assert_eq!(mgr.active_name(), "incident-triage");
    }

    #[test]
    fn manager_save_user_preset() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PresetManager::new(dir.path().to_path_buf(), None);

        let preset = DashboardPreset::new(
            "my-custom",
            PanelLayoutConfig::default_two_panel(),
            vec![BandConfig::new("main", 0)],
        );
        mgr.save(preset).unwrap();

        assert_eq!(mgr.count(), 5);
        assert_eq!(mgr.user_count(), 1);
        assert!(mgr.get("my-custom").is_some());

        // File should exist on disk.
        let path = dir.path().join("my-custom.json");
        assert!(path.exists());
    }

    #[test]
    fn manager_save_builtin_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PresetManager::new(dir.path().to_path_buf(), None);

        let preset = builtin_default();
        let result = mgr.save(preset);
        assert!(result.is_err());
    }

    #[test]
    fn manager_delete_user_preset() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PresetManager::new(dir.path().to_path_buf(), None);

        let preset =
            DashboardPreset::new("to-delete", PanelLayoutConfig::default_two_panel(), vec![]);
        mgr.save(preset).unwrap();
        assert!(mgr.get("to-delete").is_some());

        mgr.delete("to-delete").unwrap();
        assert!(mgr.get("to-delete").is_none());
        assert_eq!(mgr.count(), 4);
    }

    #[test]
    fn manager_delete_builtin_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PresetManager::new(dir.path().to_path_buf(), None);
        assert!(mgr.delete("default").is_err());
    }

    #[test]
    fn manager_delete_nonexistent_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PresetManager::new(dir.path().to_path_buf(), None);
        assert!(mgr.delete("nonexistent").is_err());
    }

    #[test]
    fn manager_delete_active_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PresetManager::new(dir.path().to_path_buf(), None);

        let preset =
            DashboardPreset::new("active-one", PanelLayoutConfig::default_two_panel(), vec![]);
        mgr.save(preset).unwrap();
        mgr.activate("active-one");
        assert_eq!(mgr.active_name(), "active-one");

        mgr.delete("active-one").unwrap();
        assert_eq!(mgr.active_name(), "default");
    }

    #[test]
    fn manager_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().to_path_buf();

        // Session 1: save a preset.
        {
            let mut mgr = PresetManager::new(storage.clone(), None);
            let preset = DashboardPreset::new(
                "persisted",
                PanelLayoutConfig::default_two_panel(),
                vec![BandConfig::new("main", 0)],
            )
            .with_description("Survives restart");
            mgr.save(preset).unwrap();
        }

        // Session 2: reload.
        {
            let mgr = PresetManager::new(storage, None);
            let loaded = mgr.get("persisted").unwrap();
            assert_eq!(loaded.description.as_deref(), Some("Survives restart"));
            assert!(!loaded.builtin);
        }
    }

    #[test]
    fn manager_list_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PresetManager::new(dir.path().to_path_buf(), None);

        // Add presets with names that should sort.
        for name in ["zulu", "alpha", "mike"] {
            mgr.save(DashboardPreset::new(
                name,
                PanelLayoutConfig::default_two_panel(),
                vec![],
            ))
            .unwrap();
        }

        let names = mgr.list();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "preset list should be sorted");
    }

    // ── Sanitize filename ───────────────────────────────────────────

    #[test]
    fn sanitize_preserves_alphanumeric() {
        assert_eq!(sanitize_filename("my-preset_v2"), "my-preset_v2");
    }

    #[test]
    fn sanitize_replaces_spaces_and_slashes() {
        assert_eq!(sanitize_filename("my preset/v2"), "my_preset_v2");
    }

    #[test]
    fn sanitize_handles_empty() {
        assert_eq!(sanitize_filename(""), "");
    }

    // ── Error display ───────────────────────────────────────────────

    #[test]
    fn error_display_builtin() {
        let err = PresetError::BuiltinReadOnly("default".into());
        assert!(err.to_string().contains("built-in"));
    }

    #[test]
    fn error_display_not_found() {
        let err = PresetError::NotFound("missing".into());
        assert!(err.to_string().contains("not found"));
    }
}
