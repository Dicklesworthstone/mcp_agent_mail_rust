//! Platform-independent host-driven runner for the browser dashboard.

use core::time::Duration;

use ftui::{Event, KeyEventKind, MouseButton, MouseEventKind};
use ftui_web::step_program::{StepProgram, StepResult};
use ftui_web::{WebFlatPatchBatch, WebPatchStats};

use crate::demo_pack::DemoPackError;
use crate::model::DashboardModel;

const MAX_HOST_ADVANCE_MS: f64 = 60_000.0;
const NANOS_PER_MILLI: u128 = 1_000_000;
const LOGICAL_TICK_MS: u64 = 100;

#[derive(Debug, Clone, serde::Serialize)]
pub struct RunnerStatus {
    pub running: bool,
    pub frame_index: u64,
    pub elapsed_ms: u64,
    pub duration_ms: u64,
    pub paused: bool,
    pub reduced_motion: bool,
    pub replay_label: String,
    pub source_label: String,
    pub content_sha256: String,
    pub projects: u64,
    pub agents: u64,
    pub messages: u64,
    pub active_reservations: u64,
    pub pending_acknowledgements: u64,
    pub last_deep_link: Option<String>,
    pub active_screen: String,
    pub dashboard_filter: String,
    pub help_visible: bool,
    pub interaction_revision: u64,
    pub selected_row: usize,
}

pub struct DashboardRunnerCore {
    inner: StepProgram<DashboardModel>,
    cached_patch_hash: Option<String>,
    cached_patch_stats: Option<WebPatchStats>,
    cached_logs: Vec<String>,
    replay_submillis_nanos: u32,
    logical_tick_remainder_ms: u64,
    render_request_pending: bool,
}

impl DashboardRunnerCore {
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            inner: StepProgram::new(DashboardModel::unloaded(), cols.max(1), rows.max(1)),
            cached_patch_hash: None,
            cached_patch_stats: None,
            cached_logs: Vec::new(),
            replay_submillis_nanos: 0,
            logical_tick_remainder_ms: 0,
            render_request_pending: false,
        }
    }

    pub fn init(&mut self) {
        if self.inner.is_initialized() {
            return;
        }
        if let Err(error) = self.inner.init() {
            self.cached_logs.push(format!("runner_init_error: {error}"));
        }
    }

    pub fn load_demo_pack_json(&mut self, json: &str) -> Result<(), DemoPackError> {
        self.inner.model_mut().load_pack_json(json)?;
        self.reset_timing_accumulators();
        self.request_render();
        Ok(())
    }

    pub fn advance_time_ms(&mut self, dt_ms: f64) {
        if !dt_ms.is_finite() || dt_ms <= 0.0 {
            return;
        }
        // Pause freezes both replay state and the runner clock that drives
        // animations. Discard paused wall time rather than replaying it later.
        if self.inner.model().paused() {
            return;
        }
        let bounded_ms = dt_ms.min(MAX_HOST_ADVANCE_MS);
        let duration =
            Duration::try_from_secs_f64(bounded_ms / 1_000.0).unwrap_or(Duration::from_secs(60));
        self.inner.advance_time(duration);

        let replay_nanos = u128::from(self.replay_submillis_nanos) + duration.as_nanos();
        let elapsed_ms = u64::try_from(replay_nanos / NANOS_PER_MILLI).unwrap_or(u64::MAX);
        self.replay_submillis_nanos =
            u32::try_from(replay_nanos % NANOS_PER_MILLI).unwrap_or_default();
        if elapsed_ms == 0 {
            return;
        }

        // Advance in logical-tick-sized slices. This keeps data ingestion and
        // stat history identical whether the host supplies many RAF deltas or
        // one large catch-up delta, while still requesting only one repaint.
        let mut remaining_ms = elapsed_ms;
        let mut visible_changed = false;
        while remaining_ms > 0 {
            let to_tick_ms = LOGICAL_TICK_MS.saturating_sub(self.logical_tick_remainder_ms);
            let slice_ms = remaining_ms.min(to_tick_ms);
            let advance = self.inner.model_mut().advance_replay(slice_ms);
            if advance.advanced_ms == 0 {
                break;
            }
            remaining_ms = remaining_ms.saturating_sub(advance.advanced_ms);
            visible_changed |= advance.visible_changed;

            if advance.replay_reset {
                // The model rebuilt the final replay cycle and reset its tick
                // history. Reconstruct only ticks in that surviving cycle.
                let replay_elapsed_ms = self.inner.model().elapsed_ms();
                let due_ticks = replay_elapsed_ms / LOGICAL_TICK_MS;
                self.logical_tick_remainder_ms = replay_elapsed_ms % LOGICAL_TICK_MS;
                for _ in 0..due_ticks {
                    self.inner.model_mut().logical_tick();
                }
                visible_changed |= due_ticks > 0;
            } else {
                self.logical_tick_remainder_ms = self
                    .logical_tick_remainder_ms
                    .saturating_add(advance.advanced_ms);
                if self.logical_tick_remainder_ms == LOGICAL_TICK_MS {
                    self.logical_tick_remainder_ms = 0;
                    self.inner.model_mut().logical_tick();
                    visible_changed = true;
                }
            }

            if advance.advanced_ms < slice_ms {
                break;
            }
        }
        if visible_changed {
            self.request_render();
        }
    }

    pub fn push_encoded_input(&mut self, json: &str) -> bool {
        match ftui_web::input_parser::parse_encoded_input_to_event(json) {
            Ok(Some(event)) if browser_event_can_change_model(&event) => {
                self.inner.push_event(event);
                true
            }
            _ => false,
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.inner.resize(cols.max(1), rows.max(1));
    }

    pub fn step(&mut self) -> StepResult {
        if !self.inner.is_initialized() {
            self.init();
        }
        self.inner.model().sync_replay_clock();
        let result = match self.inner.step() {
            Ok(result) => result,
            Err(error) => {
                self.cached_logs.push(format!("runner_step_error: {error}"));
                StepResult {
                    running: self.inner.is_running(),
                    rendered: false,
                    events_processed: 0,
                    frame_idx: self.inner.frame_idx(),
                }
            }
        };
        self.render_request_pending = false;
        result
    }

    #[must_use]
    pub fn take_flat_patches(&mut self) -> WebFlatPatchBatch {
        let mut outputs = self.inner.take_outputs();
        self.cached_patch_hash = outputs.compute_patch_hash().map(str::to_owned);
        self.cached_patch_stats = outputs.last_patch_stats;
        let patches = outputs.flatten_patches_u32();
        self.cached_logs.append(&mut outputs.logs);
        patches
    }

    #[must_use]
    pub fn patch_hash(&self) -> Option<&str> {
        self.cached_patch_hash.as_deref()
    }

    #[must_use]
    pub const fn patch_stats(&self) -> Option<WebPatchStats> {
        self.cached_patch_stats
    }

    #[must_use]
    pub fn take_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.cached_logs)
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.inner.model_mut().set_paused(paused);
    }

    pub fn set_reduced_motion(&mut self, reduced_motion: bool) {
        self.inner.model_mut().set_reduced_motion(reduced_motion);
        self.request_render();
    }

    pub fn reset(&mut self) {
        self.inner.model_mut().reset();
        self.reset_timing_accumulators();
        self.request_render();
    }

    #[must_use]
    pub fn status(&self) -> RunnerStatus {
        let model = self.inner.model();
        let stats = model
            .state()
            .db_stats_snapshot()
            .unwrap_or_else(|| model.pack().bootstrap.db_stats.clone());
        RunnerStatus {
            running: self.inner.is_running(),
            frame_index: self.inner.frame_idx(),
            elapsed_ms: model.elapsed_ms(),
            duration_ms: model.pack().duration_ms,
            paused: model.paused(),
            reduced_motion: model.reduced_motion(),
            replay_label: model.pack().replay_label.clone(),
            source_label: model.pack().provenance.source_label.clone(),
            content_sha256: model.pack().provenance.content_sha256.clone(),
            projects: stats.projects,
            agents: stats.agents,
            messages: stats.messages,
            active_reservations: stats.file_reservations,
            pending_acknowledgements: stats.ack_pending,
            last_deep_link: model.last_deep_link().map(str::to_owned),
            active_screen: model.active_screen().as_slug().to_string(),
            dashboard_filter: model.dashboard_filter_slug().to_string(),
            help_visible: model.help_visible(),
            interaction_revision: model.interaction_revision(),
            selected_row: model.public_selected_row(),
        }
    }

    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        self.inner.size()
    }

    fn reset_timing_accumulators(&mut self) {
        self.replay_submillis_nanos = 0;
        self.logical_tick_remainder_ms = 0;
    }

    fn request_render(&mut self) {
        if self.inner.is_initialized() && !self.render_request_pending {
            // StepProgram intentionally renders only after an event. Replay
            // ticks were already applied at their exact logical boundaries,
            // so a focus refresh is a mutation-free invalidation signal.
            self.inner.push_event(Event::Focus(true));
            self.render_request_pending = true;
        }
    }
}

fn browser_event_can_change_model(event: &Event) -> bool {
    match event {
        Event::Key(key) => key.kind == KeyEventKind::Press,
        Event::Paste(_) => true,
        Event::Mouse(mouse) => matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollUp
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::DashboardRunnerCore;
    use crate::demo_pack::curated_public_demo;

    fn loaded_runner(cols: u16, rows: u16) -> DashboardRunnerCore {
        let mut runner = DashboardRunnerCore::new(cols, rows);
        let json = curated_public_demo()
            .to_pretty_json()
            .expect("curated public demo should serialize");
        runner
            .load_demo_pack_json(&json)
            .expect("curated public demo should load");
        runner
    }

    fn initial_patch_contract(cols: u16, rows: u16) -> (String, usize, usize) {
        let mut runner = loaded_runner(cols, rows);
        runner.set_reduced_motion(true);
        runner.init();
        let patches = runner.take_flat_patches();
        (
            runner.patch_hash().unwrap_or_default().to_string(),
            patches.spans.len(),
            patches.cells.len(),
        )
    }

    #[test]
    fn runner_produces_initial_flat_patch_batch() {
        let mut runner = loaded_runner(120, 36);
        runner.init();
        let patches = runner.take_flat_patches();
        assert!(!patches.cells.is_empty());
        assert!(!patches.spans.is_empty());
        assert_eq!(runner.size(), (120, 36));
    }

    #[test]
    fn same_input_sequence_is_deterministic() {
        fn run() -> (String, u64) {
            let mut runner = loaded_runner(120, 36);
            runner.set_reduced_motion(true);
            runner.init();
            let _ = runner.take_flat_patches();
            runner.advance_time_ms(2_000.0);
            let _ = runner.step();
            let _ = runner.take_flat_patches();
            (
                runner.patch_hash().unwrap_or_default().to_string(),
                runner.status().elapsed_ms,
            )
        }
        assert_eq!(run(), run());
    }

    #[test]
    fn tiny_compact_and_wide_dashboard_patch_contracts_are_deterministic() {
        let mut hashes = std::collections::HashSet::new();
        for (scenario, cols, rows) in [
            ("tiny-40x12", 40, 12),
            ("compact-80x24", 80, 24),
            ("wide-160x44", 160, 44),
        ] {
            let first = initial_patch_contract(cols, rows);
            let second = initial_patch_contract(cols, rows);
            assert_eq!(
                first, second,
                "scenario={scenario} dimensions={cols}x{rows}"
            );
            assert!(
                first.0.len() >= 16,
                "scenario={scenario} missing stable patch hash"
            );
            assert!(first.1 > 0, "scenario={scenario} emitted no patch spans");
            assert!(first.2 > 0, "scenario={scenario} emitted no patch cells");
            hashes.insert(first.0);
        }
        assert_eq!(
            hashes.len(),
            3,
            "responsive layouts collapsed to one patch contract"
        );
    }

    #[test]
    fn replay_counts_remain_coherent_across_message_ack_and_release_actions() {
        let mut runner = loaded_runner(120, 36);
        runner.init();
        let baseline = runner.status();

        runner.advance_time_ms(2_000.0);
        let _ = runner.step();
        let after_message = runner.status();
        assert_eq!(after_message.messages, baseline.messages + 1);
        assert_eq!(
            after_message.pending_acknowledgements,
            baseline.pending_acknowledgements + 1
        );

        runner.advance_time_ms(4_000.0);
        let _ = runner.step();
        assert_eq!(
            runner.status().pending_acknowledgements,
            baseline.pending_acknowledgements
        );

        runner.advance_time_ms(4_000.0);
        let _ = runner.step();
        assert_eq!(
            runner.status().active_reservations,
            baseline.active_reservations - 1
        );

        runner.reset();
        assert_eq!(runner.status().messages, baseline.messages);
        assert_eq!(runner.status().elapsed_ms, 0);
    }

    #[test]
    fn browser_input_and_reduced_motion_cannot_mutate_mailbox_counts() {
        let mut runner = loaded_runner(120, 36);
        runner.set_reduced_motion(true);
        runner.set_paused(true);
        runner.init();
        let before = runner.status();
        assert!(runner.push_encoded_input(
            r#"{"kind":"key","phase":"down","key":"2","code":"Digit2","mods":0,"repeat":false}"#,
        ));
        let _ = runner.step();
        runner.advance_time_ms(15_000.0);
        let after = runner.status();
        assert_eq!(after.active_screen, "messages");
        assert!(after.interaction_revision > before.interaction_revision);
        assert!(after.reduced_motion);
        assert!(after.paused);
        assert_eq!(after.elapsed_ms, 0);
        assert_eq!(after.projects, before.projects);
        assert_eq!(after.agents, before.agents);
        assert_eq!(after.messages, before.messages);
        assert_eq!(after.active_reservations, before.active_reservations);
        assert_eq!(
            after.pending_acknowledgements,
            before.pending_acknowledgements
        );
    }

    #[test]
    fn no_op_browser_input_phases_do_not_schedule_duplicate_frames() {
        let mut runner = loaded_runner(120, 36);
        runner.init();
        let _ = runner.take_flat_patches();

        assert!(!runner.push_encoded_input(
            r#"{"kind":"key","phase":"up","key":"v","code":"KeyV","mods":0,"repeat":false}"#,
        ));
        assert!(!runner.push_encoded_input(
            r#"{"kind":"mouse","phase":"up","button":0,"x":5,"y":5,"mods":0}"#,
        ));
        assert!(!runner.push_encoded_input(r#"{"kind":"focus","focused":true}"#,));

        let step = runner.step();
        assert!(!step.rendered);
        assert_eq!(step.events_processed, 0);
    }

    #[test]
    fn rendered_native_tab_hit_region_switches_screens_from_mouse_input() {
        let mut runner = loaded_runner(220, 48);
        runner.set_paused(true);
        runner.init();
        let _ = runner.take_flat_patches();
        let before = runner.status();

        // At normal tab density Dashboard occupies cells 0..=13, followed by
        // a separator; x=18 is safely inside the rendered Messages tab.
        assert!(runner.push_encoded_input(
            r#"{"kind":"mouse","phase":"down","button":0,"x":18,"y":0,"mods":0}"#,
        ));
        let step = runner.step();
        let after = runner.status();

        assert!(step.rendered);
        assert_eq!(after.active_screen, "messages");
        assert!(after.interaction_revision > before.interaction_revision);
    }

    #[test]
    fn loading_verified_pack_after_init_schedules_a_real_frame() {
        let mut runner = DashboardRunnerCore::new(120, 36);
        runner.init();
        let _ = runner.take_flat_patches();
        let json = curated_public_demo()
            .to_pretty_json()
            .expect("curated public demo should serialize");

        runner
            .load_demo_pack_json(&json)
            .expect("verified pack should load after init");
        let step = runner.step();
        let patches = runner.take_flat_patches();

        assert!(step.rendered);
        assert!(!patches.cells.is_empty());
        assert_eq!(runner.status().active_screen, "dashboard");
    }
}
