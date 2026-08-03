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

        // If the delta crosses a loop boundary, every state mutation before
        // the final cycle is discarded by replay reset. Collapse those dead
        // cycles in O(1), then advance only the surviving cycle at native
        // logical-tick boundaries. Without this collapse, a valid 1 ms loop
        // could force hundreds of resets and bootstrap replays in one frame.
        let mut remaining_ms = elapsed_ms;
        let mut visible_changed = false;
        let (loop_replay, replay_duration_ms, replay_elapsed_ms) = {
            let model = self.inner.model();
            (
                model.pack().loop_replay,
                model.pack().duration_ms,
                model.elapsed_ms(),
            )
        };
        let to_replay_endpoint_ms = replay_duration_ms.saturating_sub(replay_elapsed_ms);
        if loop_replay && replay_duration_ms > 0 && remaining_ms > to_replay_endpoint_ms {
            let total_ms = u128::from(replay_elapsed_ms) + u128::from(remaining_ms);
            let remainder =
                (total_ms - u128::from(replay_duration_ms)) % u128::from(replay_duration_ms);
            remaining_ms = if remainder == 0 {
                replay_duration_ms
            } else {
                u64::try_from(remainder).unwrap_or(replay_duration_ms)
            };
            self.inner.model_mut().restart_looping_replay_cycle();
            self.logical_tick_remainder_ms = 0;
            visible_changed = true;
        }

        // Advance the surviving cycle in logical-tick-sized slices. This
        // keeps action ingestion and stat history identical whether the host
        // supplies many RAF deltas or one large catch-up delta, while still
        // requesting only one repaint.
        while remaining_ms > 0 {
            let to_tick_ms = LOGICAL_TICK_MS.saturating_sub(self.logical_tick_remainder_ms);
            let to_replay_endpoint_ms = {
                let model = self.inner.model();
                model.pack().duration_ms.saturating_sub(model.elapsed_ms())
            };
            let mut slice_ms = remaining_ms.min(to_tick_ms);
            if to_replay_endpoint_ms > 0 {
                // Land on a partial replay endpoint before wrapping. This
                // gives its final actions one ingestion tick before the model
                // reconstructs the surviving cycle.
                slice_ms = slice_ms.min(to_replay_endpoint_ms);
            }
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

            let at_replay_endpoint = {
                let model = self.inner.model();
                model.elapsed_ms() == model.pack().duration_ms
            };
            if at_replay_endpoint && self.logical_tick_remainder_ms > 0 {
                // A replay whose duration is not a multiple of the native
                // cadence still needs one terminal ingestion tick. Otherwise
                // endpoint actions remain absent from Dashboard caches forever.
                self.logical_tick_remainder_ms = 0;
                self.inner.model_mut().logical_tick();
                visible_changed = true;
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
    use crate::demo_pack::{DemoOperation, DemoPack, curated_public_demo};

    fn runner_with_pack(pack: &DemoPack, cols: u16, rows: u16) -> DashboardRunnerCore {
        let mut runner = DashboardRunnerCore::new(cols, rows);
        let json = pack
            .to_pretty_json()
            .expect("public demo pack should serialize");
        runner
            .load_demo_pack_json(&json)
            .expect("public demo pack should load");
        runner
    }

    fn loaded_runner(cols: u16, rows: u16) -> DashboardRunnerCore {
        runner_with_pack(&curated_public_demo(), cols, rows)
    }

    fn terminal_action_pack(loop_replay: bool) -> DemoPack {
        let mut pack = curated_public_demo();
        let mut terminal_action = pack
            .actions
            .iter()
            .find(|action| {
                action.at_ms > 0 && matches!(&action.operation, DemoOperation::PublishEvent { .. })
            })
            .cloned()
            .expect("curated demo should include a timed public event");
        terminal_action.at_ms = 150;
        pack.actions.retain(|action| action.at_ms == 0);
        pack.actions.push(terminal_action);
        pack.duration_ms = 150;
        pack.loop_replay = loop_replay;
        pack.finalize_digest();
        pack
    }

    fn latest_dashboard_raw_count(runner: &DashboardRunnerCore) -> u64 {
        runner
            .inner
            .model()
            .state()
            .screen_diagnostics_since(0)
            .into_iter()
            .filter(|(_, diagnostic)| diagnostic.screen == "dashboard")
            .map(|(_, diagnostic)| diagnostic.raw_count)
            .next_back()
            .expect("Dashboard render should emit a diagnostic")
    }

    fn assert_terminal_partial_tick(loop_replay: bool) {
        let pack = terminal_action_pack(loop_replay);
        let mut runner = runner_with_pack(&pack, 120, 36);
        runner.set_reduced_motion(true);
        runner.init();
        let baseline_raw_count = latest_dashboard_raw_count(&runner);

        runner.advance_time_ms(150.0);
        let step = runner.step();

        assert!(step.rendered);
        assert_eq!(runner.status().elapsed_ms, 150);
        assert_eq!(runner.inner.model().tick_count(), 12);
        assert_eq!(runner.logical_tick_remainder_ms, 0);
        assert_eq!(latest_dashboard_raw_count(&runner), baseline_raw_count + 1);
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
        assert_eq!(runner.inner.model().tick_count(), 10);

        runner.advance_time_ms(2_000.0);
        let _ = runner.step();
        let after_message = runner.status();
        assert_eq!(runner.inner.model().tick_count(), 30);
        assert_eq!(after_message.messages, baseline.messages + 1);
        assert_eq!(
            after_message.pending_acknowledgements,
            baseline.pending_acknowledgements + 1
        );

        runner.advance_time_ms(4_000.0);
        let _ = runner.step();
        assert_eq!(runner.inner.model().tick_count(), 70);
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
        assert_eq!(after.dashboard_filter, "all");
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

    #[test]
    fn fractional_animation_frames_accumulate_without_replay_drift() {
        let mut runner = loaded_runner(120, 36);
        runner.set_reduced_motion(true);
        runner.init();
        let _ = runner.take_flat_patches();
        let mut rendered_frames = 0;

        for _ in 0..60 {
            runner.advance_time_ms(16.666_7);
            rendered_frames += usize::from(runner.step().rendered);
        }

        assert_eq!(runner.status().elapsed_ms, 1_000);
        assert_eq!(runner.inner.model().tick_count(), 20);
        assert_eq!(rendered_frames, 10);
    }

    #[test]
    fn large_and_small_advances_apply_the_same_logical_ticks() {
        fn run(deltas: impl IntoIterator<Item = f64>) -> (u64, u64, u64, u64) {
            let mut runner = loaded_runner(120, 36);
            runner.set_reduced_motion(true);
            runner.init();
            for delta in deltas {
                runner.advance_time_ms(delta);
                let _ = runner.step();
            }
            let status = runner.status();
            (
                status.elapsed_ms,
                runner.inner.model().tick_count(),
                status.messages,
                status.pending_acknowledgements,
            )
        }

        assert_eq!(run([2_000.0]), run(std::iter::repeat_n(100.0, 20)));
    }

    #[test]
    fn huge_looping_advance_keeps_only_final_cycle_ticks_and_one_repaint() {
        let mut runner = loaded_runner(120, 36);
        runner.set_reduced_motion(true);
        runner.init();
        let _ = runner.take_flat_patches();

        runner.advance_time_ms(60_000.0);
        let step = runner.step();

        assert_eq!(runner.status().elapsed_ms, 6_000);
        assert_eq!(runner.inner.model().tick_count(), 70);
        assert!(step.rendered);
        assert_eq!(step.events_processed, 1);
    }

    #[test]
    fn non_looping_partial_endpoint_gets_a_terminal_ingestion_tick() {
        assert_terminal_partial_tick(false);
    }

    #[test]
    fn looping_partial_endpoint_gets_a_terminal_ingestion_tick() {
        assert_terminal_partial_tick(true);
    }

    #[test]
    fn looping_cross_endpoint_flushes_then_reconstructs_the_surviving_cycle() {
        let pack = terminal_action_pack(true);
        let mut runner = runner_with_pack(&pack, 120, 36);
        runner.set_reduced_motion(true);
        runner.init();
        let baseline_raw_count = latest_dashboard_raw_count(&runner);

        // This single host delta crosses the 150 ms endpoint. The runner can
        // discard that completed cycle because reset removes all of its state,
        // then reconstruct the surviving cycle at 50 ms without carrying the
        // prior cycle's caches or cadence.
        runner.advance_time_ms(200.0);
        let crossed = runner.step();

        assert!(crossed.rendered);
        assert_eq!(crossed.events_processed, 1);
        assert_eq!(runner.status().elapsed_ms, 50);
        assert_eq!(runner.inner.model().tick_count(), 10);
        assert_eq!(runner.logical_tick_remainder_ms, 50);
        assert_eq!(latest_dashboard_raw_count(&runner), baseline_raw_count);

        // Finishing the surviving cycle proves its partial endpoint still
        // receives both the 100 ms cadence tick and the terminal flush.
        runner.advance_time_ms(100.0);
        let endpoint = runner.step();

        assert!(endpoint.rendered);
        assert_eq!(endpoint.events_processed, 1);
        assert_eq!(runner.status().elapsed_ms, 150);
        assert_eq!(runner.inner.model().tick_count(), 12);
        assert_eq!(runner.logical_tick_remainder_ms, 0);
        assert_eq!(latest_dashboard_raw_count(&runner), baseline_raw_count + 1);
    }

    #[test]
    fn dense_one_millisecond_loop_collapses_catch_up_to_one_reset() {
        let mut pack = curated_public_demo();
        let startup_action = pack
            .actions
            .iter()
            .find(|action| action.at_ms == 0)
            .cloned()
            .expect("curated demo should include a startup action");
        pack.actions = vec![startup_action; 512];
        pack.duration_ms = 1;
        pack.loop_replay = true;
        pack.finalize_digest();
        pack.validate().expect("dense short loop should be valid");

        let mut runner = runner_with_pack(&pack, 120, 36);
        runner.set_reduced_motion(true);
        runner.init();
        let resets_before = runner.inner.model().replay_reset_count();

        runner.advance_time_ms(60_000.0);
        let step = runner.step();

        assert!(step.rendered);
        assert_eq!(runner.status().elapsed_ms, 1);
        assert_eq!(runner.inner.model().tick_count(), 11);
        assert_eq!(
            runner.inner.model().replay_reset_count(),
            resets_before + 1,
            "discarded loop cycles must not each clone and reapply the bootstrap"
        );
    }

    #[test]
    fn interleaved_runners_install_their_own_replay_clock_before_step() {
        let mut advanced = loaded_runner(120, 36);
        let mut initial = loaded_runner(120, 36);
        advanced.init();
        initial.init();
        let base = advanced
            .inner
            .model()
            .pack()
            .bootstrap
            .db_stats
            .timestamp_micros;

        advanced.advance_time_ms(2_500.0);
        let _ = advanced.step();
        assert_eq!(
            crate::tui_screens::dashboard::browser_replay_now_micros(),
            Some(base + 2_500_000)
        );

        let idle_step = initial.step();
        assert!(!idle_step.rendered);
        assert_eq!(
            crate::tui_screens::dashboard::browser_replay_now_micros(),
            Some(base)
        );
    }

    #[test]
    fn pause_freezes_replay_and_the_underlying_runner_clock() {
        let mut runner = loaded_runner(120, 36);
        runner.init();
        runner.set_paused(true);
        let backend_before = format!("{:?}", runner.inner.backend());

        runner.advance_time_ms(15_000.75);

        assert_eq!(runner.status().elapsed_ms, 0);
        assert_eq!(runner.inner.model().tick_count(), 10);
        assert_eq!(format!("{:?}", runner.inner.backend()), backend_before);
        assert!(!runner.step().rendered);
    }

    #[test]
    fn ctrl_p_opens_browser_search_even_from_dashboard_text_mode() {
        let mut runner = loaded_runner(120, 36);
        runner.init();
        assert!(runner.push_encoded_input(
            r#"{"kind":"key","phase":"down","key":"/","code":"Slash","mods":0,"repeat":false}"#,
        ));
        let _ = runner.step();
        assert_eq!(runner.status().active_screen, "dashboard");

        assert!(runner.push_encoded_input(
            r#"{"kind":"key","phase":"down","key":"p","code":"KeyP","mods":4,"repeat":false}"#,
        ));
        let _ = runner.step();

        assert_eq!(runner.status().active_screen, "search");
        assert_eq!(runner.status().last_deep_link.as_deref(), Some("search:"));
    }
}
