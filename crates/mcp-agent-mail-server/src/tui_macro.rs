//! Operator macro recorder and deterministic playback engine (br-3vwi.8.4).
//!
//! Lets operators capture multi-step interaction macros directly from UI
//! behavior, then replay with deterministic ordering and visible execution
//! logs. Includes guardrails: dry-run preview, per-step confirmation mode,
//! and failure-stop semantics.
//!
//! Macros are persisted as versioned JSON artifacts for sharing and
//! reproducible runbooks.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Macro Step ─────────────────────────────────────────────────────────

/// A single recorded step in a macro.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MacroStep {
    /// Palette action ID (e.g. "screen:Messages", "macro:view_thread:t1").
    pub action_id: String,
    /// Human-readable label for display in playback log.
    pub label: String,
    /// Delay in milliseconds from the previous step (for timing fidelity).
    pub delay_ms: u64,
}

impl MacroStep {
    /// Create a new step from a palette action.
    #[must_use]
    pub fn new(action_id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            action_id: action_id.into(),
            label: label.into(),
            delay_ms: 0,
        }
    }

    /// Set the delay from the previous step.
    #[must_use]
    pub fn with_delay(mut self, ms: u64) -> Self {
        self.delay_ms = ms;
        self
    }

    /// Stable 64-bit hash for determinism checks and replay forensics.
    ///
    /// Intentionally excludes timestamps and timing (`delay_ms`) so hashes are
    /// resilient to variable operator pacing.
    #[must_use]
    pub fn stable_hash64(&self) -> u64 {
        stable_hash64_pair(&self.action_id, &self.label)
    }
}

// ── Macro Definition ───────────────────────────────────────────────────

/// Schema version for forward compatibility.
const SCHEMA_VERSION: u32 = 1;

/// A named, versioned macro definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacroDef {
    /// Schema version for forward compatibility.
    pub version: u32,
    /// Unique macro name (used as key and filename slug).
    pub name: String,
    /// Optional description for runbook documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Ordered steps to execute.
    pub steps: Vec<MacroStep>,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// ISO-8601 last-modified timestamp.
    pub updated_at: String,
}

impl MacroDef {
    /// Create a new macro definition.
    #[must_use]
    pub fn new(name: impl Into<String>, steps: Vec<MacroStep>) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            version: SCHEMA_VERSION,
            name: name.into(),
            description: None,
            steps,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// Set a description.
    #[must_use]
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Number of steps.
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether the macro has no steps.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

// ── Playback Mode ──────────────────────────────────────────────────────

/// Controls how the macro engine executes playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackMode {
    /// Execute all steps without pausing.
    Continuous,
    /// Pause after each step and wait for confirmation.
    StepByStep,
    /// Show steps without executing (preview mode).
    DryRun,
}

// ── Playback State ─────────────────────────────────────────────────────

/// Current state of macro playback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackState {
    /// No macro is playing.
    Idle,
    /// Playing a macro: (macro name, current step index, mode).
    Playing {
        name: String,
        step: usize,
        total: usize,
        mode: PlaybackMode,
    },
    /// Paused at a step waiting for confirmation (step-by-step mode).
    Paused {
        name: String,
        step: usize,
        total: usize,
    },
    /// Playback completed successfully.
    Completed { name: String, steps_executed: usize },
    /// Playback failed at a step.
    Failed {
        name: String,
        step: usize,
        reason: String,
    },
}

impl PlaybackState {
    /// Whether playback is active (playing or paused).
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Playing { .. } | Self::Paused { .. })
    }

    /// Short status string for the chrome status bar.
    #[must_use]
    pub fn status_label(&self) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Playing {
                name, step, total, ..
            } => Some(format!("Playing {name} [{}/{total}]", step + 1)),
            Self::Paused {
                name, step, total, ..
            } => Some(format!(
                "Paused {name} [{}/{total}] (Enter=next, Esc=stop)",
                step + 1
            )),
            Self::Completed {
                name,
                steps_executed,
            } => Some(format!("Done {name} ({steps_executed} steps)")),
            Self::Failed { name, step, reason } => {
                Some(format!("Failed {name} at step {} ({reason})", step + 1))
            }
        }
    }
}

// ── Recorder State ─────────────────────────────────────────────────────

/// State of the macro recorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecorderState {
    /// Not recording.
    Idle,
    /// Recording steps.
    Recording { step_count: usize },
}

impl RecorderState {
    /// Whether the recorder is active.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        matches!(self, Self::Recording { .. })
    }
}

// ── Macro Engine ───────────────────────────────────────────────────────

/// The core macro engine: manages recording, playback, and persistence.
pub struct MacroEngine {
    /// Stored macros indexed by name.
    macros: BTreeMap<String, MacroDef>,
    /// Directory for macro JSON files.
    storage_dir: PathBuf,
    /// Current recorder state.
    recorder: RecorderState,
    /// Steps being recorded.
    recording_buffer: Vec<MacroStep>,
    /// Timestamp of the last recorded step (for computing delays).
    last_step_ts: Option<std::time::Instant>,
    /// Current playback state.
    playback: PlaybackState,
    /// The macro being played back (cloned for ownership).
    playback_macro: Option<MacroDef>,
    /// Log of executed steps during current playback.
    playback_log: Vec<PlaybackLogEntry>,
}

/// An entry in the playback execution log.
#[derive(Debug, Clone, Serialize)]
pub struct PlaybackLogEntry {
    pub step_index: usize,
    pub action_id: String,
    pub label: String,
    pub executed: bool,
    pub skipped: bool,
    pub error: Option<String>,
}

impl MacroEngine {
    /// Create a new engine with default storage directory.
    #[must_use]
    pub fn new() -> Self {
        let storage_dir = default_macro_dir();
        let mut engine = Self {
            macros: BTreeMap::new(),
            storage_dir,
            recorder: RecorderState::Idle,
            recording_buffer: Vec::new(),
            last_step_ts: None,
            playback: PlaybackState::Idle,
            playback_macro: None,
            playback_log: Vec::new(),
        };
        engine.load_all();
        engine
    }

    /// Create with a custom storage directory (for testing).
    #[must_use]
    pub fn with_dir(dir: PathBuf) -> Self {
        let mut engine = Self {
            macros: BTreeMap::new(),
            storage_dir: dir,
            recorder: RecorderState::Idle,
            recording_buffer: Vec::new(),
            last_step_ts: None,
            playback: PlaybackState::Idle,
            playback_macro: None,
            playback_log: Vec::new(),
        };
        engine.load_all();
        engine
    }

    // ── Accessor methods ───────────────────────────────────────────

    /// Current recorder state.
    #[must_use]
    pub fn recorder_state(&self) -> &RecorderState {
        &self.recorder
    }

    /// Current playback state.
    #[must_use]
    pub fn playback_state(&self) -> &PlaybackState {
        &self.playback
    }

    /// List all stored macro names (sorted).
    #[must_use]
    pub fn list_macros(&self) -> Vec<&str> {
        self.macros.keys().map(String::as_str).collect()
    }

    /// Get a macro by name.
    #[must_use]
    pub fn get_macro(&self, name: &str) -> Option<&MacroDef> {
        self.macros.get(name)
    }

    /// Number of stored macros.
    #[must_use]
    pub fn macro_count(&self) -> usize {
        self.macros.len()
    }

    /// Get the current playback log.
    #[must_use]
    pub fn playback_log(&self) -> &[PlaybackLogEntry] {
        &self.playback_log
    }

    /// Mark the most recent playback log entry as failed (for forensic artifacts).
    pub fn mark_last_playback_error(&mut self, error: impl Into<String>) {
        if let Some(last) = self.playback_log.last_mut() {
            last.error = Some(error.into());
        }
    }

    // ── Recording ──────────────────────────────────────────────────

    /// Start recording a new macro.
    pub fn start_recording(&mut self) {
        self.recording_buffer.clear();
        self.last_step_ts = Some(std::time::Instant::now());
        self.recorder = RecorderState::Recording { step_count: 0 };
    }

    /// Record a step during an active recording session.
    ///
    /// Ignores the call if not currently recording.
    pub fn record_step(&mut self, action_id: &str, label: &str) {
        if !self.recorder.is_recording() {
            return;
        }

        // Skip recording macro engine control actions themselves.
        if action_id.starts_with("app:macro_") {
            return;
        }

        let delay_ms = self
            .last_step_ts
            .map(|ts| ts.elapsed().as_millis() as u64)
            .unwrap_or(0);
        self.last_step_ts = Some(std::time::Instant::now());

        self.recording_buffer.push(MacroStep {
            action_id: action_id.to_string(),
            label: label.to_string(),
            delay_ms,
        });

        self.recorder = RecorderState::Recording {
            step_count: self.recording_buffer.len(),
        };
    }

    /// Stop recording and save the macro with the given name.
    ///
    /// Returns the saved macro definition, or `None` if no steps were recorded.
    pub fn stop_recording(&mut self, name: &str) -> Option<MacroDef> {
        if !self.recorder.is_recording() {
            return None;
        }

        self.recorder = RecorderState::Idle;
        self.last_step_ts = None;

        if self.recording_buffer.is_empty() {
            return None;
        }

        let steps = std::mem::take(&mut self.recording_buffer);
        let def = MacroDef::new(name, steps);
        self.macros.insert(name.to_string(), def.clone());
        self.save_macro(&def);
        Some(def)
    }

    /// Cancel recording without saving.
    pub fn cancel_recording(&mut self) {
        self.recorder = RecorderState::Idle;
        self.recording_buffer.clear();
        self.last_step_ts = None;
    }

    // ── Playback ───────────────────────────────────────────────────

    /// Start playback of a named macro.
    ///
    /// Returns `false` if the macro doesn't exist or is empty.
    pub fn start_playback(&mut self, name: &str, mode: PlaybackMode) -> bool {
        let Some(def) = self.macros.get(name).cloned() else {
            return false;
        };
        if def.is_empty() {
            return false;
        }

        let total = def.len();
        self.playback_macro = Some(def);
        self.playback_log.clear();

        if mode == PlaybackMode::StepByStep {
            self.playback = PlaybackState::Paused {
                name: name.to_string(),
                step: 0,
                total,
            };
        } else {
            self.playback = PlaybackState::Playing {
                name: name.to_string(),
                step: 0,
                total,
                mode,
            };
        }
        true
    }

    /// Get the next action to execute during playback.
    ///
    /// For `Continuous` mode: returns the next step's action_id and advances.
    /// For `StepByStep` mode: returns the next step after confirmation.
    /// For `DryRun` mode: returns step info without executing.
    ///
    /// Returns `None` when playback is not active or all steps are done.
    pub fn next_step(&mut self) -> Option<(String, PlaybackMode)> {
        let mac = self.playback_macro.as_ref()?;

        match &self.playback {
            PlaybackState::Playing {
                name,
                step,
                total,
                mode,
            } => {
                let idx = *step;
                let name = name.clone();
                let total = *total;
                let mode = *mode;

                if idx >= mac.steps.len() {
                    self.playback = PlaybackState::Completed {
                        name,
                        steps_executed: idx,
                    };
                    return None;
                }

                let step_def = &mac.steps[idx];
                let action_id = step_def.action_id.clone();

                // Log the step
                self.playback_log.push(PlaybackLogEntry {
                    step_index: idx,
                    action_id: action_id.clone(),
                    label: step_def.label.clone(),
                    executed: mode != PlaybackMode::DryRun,
                    skipped: false,
                    error: None,
                });

                // Advance to next step
                let next = idx + 1;
                if next >= total {
                    self.playback = PlaybackState::Completed {
                        name,
                        steps_executed: next,
                    };
                } else {
                    self.playback = PlaybackState::Playing {
                        name,
                        step: next,
                        total,
                        mode,
                    };
                }

                Some((action_id, mode))
            }
            _ => None,
        }
    }

    /// Confirm the current step in step-by-step mode.
    ///
    /// Returns the action_id to execute.
    pub fn confirm_step(&mut self) -> Option<String> {
        let mac = self.playback_macro.as_ref()?;

        if let PlaybackState::Paused { name, step, total } = &self.playback {
            let idx = *step;
            let name = name.clone();
            let total = *total;

            if idx >= mac.steps.len() {
                self.playback = PlaybackState::Completed {
                    name,
                    steps_executed: idx,
                };
                return None;
            }

            let step_def = &mac.steps[idx];
            let action_id = step_def.action_id.clone();

            self.playback_log.push(PlaybackLogEntry {
                step_index: idx,
                action_id: action_id.clone(),
                label: step_def.label.clone(),
                executed: true,
                skipped: false,
                error: None,
            });

            let next = idx + 1;
            if next >= total {
                self.playback = PlaybackState::Completed {
                    name,
                    steps_executed: next,
                };
            } else {
                self.playback = PlaybackState::Paused {
                    name,
                    step: next,
                    total,
                };
            }

            Some(action_id)
        } else {
            None
        }
    }

    /// Stop playback (cancel or finish early).
    pub fn stop_playback(&mut self) {
        // Only meaningful while actively playing/paused.
        if !self.playback.is_active() {
            return;
        }
        let Some(mac) = self.playback_macro.as_ref() else {
            return;
        };
        let step = self.playback_log.last().map(|e| e.step_index).unwrap_or(0);
        self.playback = PlaybackState::Failed {
            name: mac.name.clone(),
            step,
            reason: "cancelled by operator".to_string(),
        };
        self.playback_macro = None;
    }

    /// Mark playback as failed at the current step.
    pub fn fail_playback(&mut self, reason: &str) {
        // Allow failure even if the engine already advanced to Completed for the
        // last dispatched step (continuous mode advances before dispatch).
        if matches!(self.playback, PlaybackState::Idle) {
            return;
        }
        let Some(mac) = self.playback_macro.as_ref() else {
            return;
        };
        let step = self.playback_log.last().map(|e| e.step_index).unwrap_or(0);
        self.playback = PlaybackState::Failed {
            name: mac.name.clone(),
            step,
            reason: reason.to_string(),
        };
        self.playback_macro = None;
    }

    /// Clear terminal playback state (Completed/Failed → Idle).
    pub fn clear_playback(&mut self) {
        if !self.playback.is_active() {
            self.playback = PlaybackState::Idle;
            self.playback_macro = None;
            self.playback_log.clear();
        }
    }

    // ── Macro management ───────────────────────────────────────────

    /// Delete a macro by name.
    pub fn delete_macro(&mut self, name: &str) -> bool {
        if self.macros.remove(name).is_some() {
            let path = self.macro_path(name);
            let _ = std::fs::remove_file(path);
            true
        } else {
            false
        }
    }

    /// Rename a macro.
    pub fn rename_macro(&mut self, old_name: &str, new_name: &str) -> bool {
        if let Some(mut def) = self.macros.remove(old_name) {
            // Delete old file
            let old_path = self.macro_path(old_name);
            let _ = std::fs::remove_file(old_path);

            def.name = new_name.to_string();
            def.updated_at = chrono::Utc::now().to_rfc3339();
            self.macros.insert(new_name.to_string(), def.clone());
            self.save_macro(&def);
            true
        } else {
            false
        }
    }

    // ── Persistence ────────────────────────────────────────────────

    fn macro_path(&self, name: &str) -> PathBuf {
        let safe_name = sanitize_filename(name);
        self.storage_dir.join(format!("{safe_name}.json"))
    }

    fn save_macro(&self, def: &MacroDef) {
        let _ = std::fs::create_dir_all(&self.storage_dir);
        let path = self.macro_path(&def.name);
        if let Ok(json) = serde_json::to_string_pretty(def) {
            let _ = std::fs::write(path, json);
        }
    }

    fn load_all(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.storage_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Ok(data) = std::fs::read_to_string(&path) {
                    if let Ok(def) = serde_json::from_str::<MacroDef>(&data) {
                        self.macros.insert(def.name.clone(), def);
                    }
                }
            }
        }
    }

    /// Dry-run preview: returns the steps that would be executed.
    #[must_use]
    pub fn preview(&self, name: &str) -> Option<Vec<&MacroStep>> {
        self.macros.get(name).map(|def| def.steps.iter().collect())
    }
}

impl Default for MacroEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Default macro storage directory.
fn default_macro_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mcp-agent-mail")
        .join("macros")
}

/// Sanitize a macro name for use as a filename.
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

#[must_use]
fn stable_hash64_pair(a: &str, b: &str) -> u64 {
    // FNV-1a 64-bit (simple and stable; no extra deps).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in a.as_bytes() {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    // Separator to avoid accidental concatenation collisions.
    h ^= u64::from(0x1f_u8);
    h = h.wrapping_mul(0x0100_0000_01b3);
    for &byte in b.as_bytes() {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

// ── Palette action IDs for macro operations ────────────────────────────

/// Palette action ID constants for macro operations.
pub mod action_ids {
    /// Start recording a new macro.
    pub const RECORD_START: &str = "app:macro_record_start";
    /// Stop recording and prompt for name.
    pub const RECORD_STOP: &str = "app:macro_record_stop";
    /// Cancel recording without saving.
    pub const RECORD_CANCEL: &str = "app:macro_record_cancel";
    /// Prefix for playing a specific macro.
    pub const PLAY_PREFIX: &str = "app:macro_play:";
    /// Prefix for playing a macro in step-by-step mode.
    pub const PLAY_STEP_PREFIX: &str = "app:macro_play_step:";
    /// Prefix for dry-run preview.
    pub const DRY_RUN_PREFIX: &str = "app:macro_dry_run:";
    /// Prefix for deleting a specific macro.
    pub const DELETE_PREFIX: &str = "app:macro_delete:";
    /// Stop current playback.
    pub const PLAYBACK_STOP: &str = "app:macro_playback_stop";
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn test_engine() -> MacroEngine {
        let dir = tempfile::tempdir().expect("tempdir");
        MacroEngine::with_dir(dir.keep())
    }

    // ── Recording ──────────────────────────────────────────────────

    #[test]
    fn record_and_save() {
        let mut engine = test_engine();
        engine.start_recording();
        assert!(engine.recorder_state().is_recording());

        engine.record_step("screen:Messages", "Go to Messages");
        engine.record_step("macro:view_thread:t1", "View thread t1");
        engine.record_step("screen:Agents", "Go to Agents");

        let def = engine.stop_recording("my-workflow").unwrap();
        assert_eq!(def.name, "my-workflow");
        assert_eq!(def.steps.len(), 3);
        assert_eq!(def.steps[0].action_id, "screen:Messages");
        assert!(!engine.recorder_state().is_recording());
    }

    #[test]
    fn record_empty_yields_none() {
        let mut engine = test_engine();
        engine.start_recording();
        let result = engine.stop_recording("empty");
        assert!(result.is_none());
        assert_eq!(engine.macro_count(), 0);
    }

    #[test]
    fn record_skips_control_actions() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("app:macro_record_stop", "Stop recording");
        engine.record_step("screen:Messages", "Go to Messages");
        let def = engine.stop_recording("filtered").unwrap();
        assert_eq!(def.steps.len(), 1);
        assert_eq!(def.steps[0].action_id, "screen:Messages");
    }

    #[test]
    fn cancel_recording() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("screen:Messages", "Go to Messages");
        engine.cancel_recording();
        assert!(!engine.recorder_state().is_recording());
        assert_eq!(engine.macro_count(), 0);
    }

    #[test]
    fn record_when_not_recording_is_noop() {
        let mut engine = test_engine();
        engine.record_step("screen:Messages", "Go to Messages");
        // Should not crash or record anything
        assert!(!engine.recorder_state().is_recording());
    }

    // ── Playback (Continuous) ──────────────────────────────────────

    #[test]
    fn playback_continuous() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("screen:Messages", "Go to Messages");
        engine.record_step("screen:Threads", "Go to Threads");
        engine.stop_recording("workflow");

        assert!(engine.start_playback("workflow", PlaybackMode::Continuous));

        let (action1, mode1) = engine.next_step().unwrap();
        assert_eq!(action1, "screen:Messages");
        assert_eq!(mode1, PlaybackMode::Continuous);

        let (action2, _) = engine.next_step().unwrap();
        assert_eq!(action2, "screen:Threads");

        // No more steps
        assert!(engine.next_step().is_none());
        assert!(matches!(
            engine.playback_state(),
            PlaybackState::Completed {
                steps_executed: 2,
                ..
            }
        ));
    }

    #[test]
    fn playback_nonexistent_returns_false() {
        let mut engine = test_engine();
        assert!(!engine.start_playback("no-such-macro", PlaybackMode::Continuous));
    }

    // ── Playback (Step-by-Step) ────────────────────────────────────

    #[test]
    fn playback_step_by_step() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("screen:Messages", "Go to Messages");
        engine.record_step("screen:Threads", "Go to Threads");
        engine.stop_recording("steps");

        assert!(engine.start_playback("steps", PlaybackMode::StepByStep));
        assert!(matches!(
            engine.playback_state(),
            PlaybackState::Paused { step: 0, .. }
        ));

        // next_step returns None when paused (need confirm_step)
        assert!(engine.next_step().is_none());

        let action1 = engine.confirm_step().unwrap();
        assert_eq!(action1, "screen:Messages");

        let action2 = engine.confirm_step().unwrap();
        assert_eq!(action2, "screen:Threads");

        assert!(engine.confirm_step().is_none());
        assert!(matches!(
            engine.playback_state(),
            PlaybackState::Completed { .. }
        ));
    }

    // ── Playback (Dry Run) ─────────────────────────────────────────

    #[test]
    fn playback_dry_run() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("screen:Messages", "Go to Messages");
        engine.stop_recording("dry");

        assert!(engine.start_playback("dry", PlaybackMode::DryRun));
        let (action, mode) = engine.next_step().unwrap();
        assert_eq!(action, "screen:Messages");
        assert_eq!(mode, PlaybackMode::DryRun);

        // Check log records dry-run (not executed)
        assert!(!engine.playback_log()[0].executed);
    }

    // ── Stop/Fail playback ─────────────────────────────────────────

    #[test]
    fn stop_playback() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("screen:Messages", "Go to Messages");
        engine.stop_recording("cancel-test");

        engine.start_playback("cancel-test", PlaybackMode::Continuous);
        engine.stop_playback();
        assert!(matches!(
            engine.playback_state(),
            PlaybackState::Failed { reason, .. } if reason == "cancelled by operator"
        ));
    }

    #[test]
    fn fail_playback() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("screen:Messages", "Go to Messages");
        engine.stop_recording("fail-test");

        engine.start_playback("fail-test", PlaybackMode::Continuous);
        engine.fail_playback("screen not available");
        assert!(matches!(
            engine.playback_state(),
            PlaybackState::Failed { reason, .. } if reason == "screen not available"
        ));
    }

    #[test]
    fn clear_playback_after_completion() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("screen:Messages", "Go to Messages");
        engine.stop_recording("clear-test");

        engine.start_playback("clear-test", PlaybackMode::Continuous);
        engine.next_step(); // complete
        engine.clear_playback();
        assert_eq!(*engine.playback_state(), PlaybackState::Idle);
        assert!(engine.playback_log().is_empty());
    }

    // ── Persistence ────────────────────────────────────────────────

    #[test]
    fn persistence_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path().to_path_buf();

        // Create and save
        {
            let mut engine = MacroEngine::with_dir(dir_path.clone());
            engine.start_recording();
            engine.record_step("screen:Messages", "Go to Messages");
            engine.record_step("screen:Threads", "Go to Threads");
            engine.stop_recording("persisted");
        }

        // Load in a new engine
        {
            let engine = MacroEngine::with_dir(dir_path);
            let names = engine.list_macros();
            assert_eq!(names, vec!["persisted"]);

            let def = engine.get_macro("persisted").unwrap();
            assert_eq!(def.steps.len(), 2);
            assert_eq!(def.steps[0].action_id, "screen:Messages");
        }
    }

    #[test]
    fn delete_macro() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("screen:Messages", "Go to Messages");
        engine.stop_recording("deleteme");

        assert_eq!(engine.macro_count(), 1);
        assert!(engine.delete_macro("deleteme"));
        assert_eq!(engine.macro_count(), 0);
        assert!(!engine.delete_macro("deleteme")); // already gone
    }

    #[test]
    fn rename_macro() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("screen:Messages", "Go to Messages");
        engine.stop_recording("old-name");

        assert!(engine.rename_macro("old-name", "new-name"));
        assert!(engine.get_macro("old-name").is_none());
        assert_eq!(engine.get_macro("new-name").unwrap().name, "new-name");
    }

    // ── Preview ────────────────────────────────────────────────────

    #[test]
    fn preview_returns_steps() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("screen:Messages", "Go to Messages");
        engine.record_step("screen:Threads", "Go to Threads");
        engine.stop_recording("preview-test");

        let steps = engine.preview("preview-test").unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].action_id, "screen:Messages");
    }

    #[test]
    fn preview_nonexistent_returns_none() {
        let engine = test_engine();
        assert!(engine.preview("nope").is_none());
    }

    // ── Status labels ──────────────────────────────────────────────

    #[test]
    fn status_labels() {
        assert!(PlaybackState::Idle.status_label().is_none());
        assert!(
            PlaybackState::Playing {
                name: "test".into(),
                step: 0,
                total: 3,
                mode: PlaybackMode::Continuous,
            }
            .status_label()
            .unwrap()
            .contains("Playing test")
        );

        assert!(
            PlaybackState::Paused {
                name: "test".into(),
                step: 1,
                total: 3,
            }
            .status_label()
            .unwrap()
            .contains("Paused")
        );

        assert!(
            PlaybackState::Completed {
                name: "test".into(),
                steps_executed: 3,
            }
            .status_label()
            .unwrap()
            .contains("Done")
        );

        assert!(
            PlaybackState::Failed {
                name: "test".into(),
                step: 1,
                reason: "oops".into(),
            }
            .status_label()
            .unwrap()
            .contains("Failed")
        );
    }

    // ── Filename sanitization ──────────────────────────────────────

    #[test]
    fn sanitize_filenames() {
        assert_eq!(sanitize_filename("my-macro"), "my-macro");
        assert_eq!(sanitize_filename("hello world"), "hello_world");
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_filename("test_123"), "test_123");
    }

    // ── MacroStep construction ─────────────────────────────────────

    #[test]
    fn step_builder() {
        let step = MacroStep::new("screen:Messages", "Go to Messages").with_delay(500);
        assert_eq!(step.action_id, "screen:Messages");
        assert_eq!(step.delay_ms, 500);
    }

    // ── MacroDef construction ──────────────────────────────────────

    #[test]
    fn macro_def_builder() {
        let def = MacroDef::new(
            "test",
            vec![MacroStep::new("screen:Messages", "Go to Messages")],
        )
        .with_description("A test macro");

        assert_eq!(def.name, "test");
        assert_eq!(def.len(), 1);
        assert!(!def.is_empty());
        assert_eq!(def.description.as_deref(), Some("A test macro"));
        assert_eq!(def.version, SCHEMA_VERSION);
    }

    // ── Playback log ───────────────────────────────────────────────

    #[test]
    fn playback_log_populated() {
        let mut engine = test_engine();
        engine.start_recording();
        engine.record_step("screen:Messages", "Go to Messages");
        engine.record_step("screen:Threads", "Go to Threads");
        engine.stop_recording("log-test");

        engine.start_playback("log-test", PlaybackMode::Continuous);
        engine.next_step();
        engine.next_step();

        let log = engine.playback_log();
        assert_eq!(log.len(), 2);
        assert!(log[0].executed);
        assert_eq!(log[0].action_id, "screen:Messages");
        assert_eq!(log[1].action_id, "screen:Threads");
    }
}
