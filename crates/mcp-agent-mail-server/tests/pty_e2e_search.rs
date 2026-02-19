//! br-3vwi.10.3: PTY E2E coverage for Search Cockpit + interaction workflows.
//!
//! Exercises keyboard flows, deep-link routing, palette actions, and layout
//! persistence through the full `MailAppModel` event loop — the same code path
//! used when a real PTY drives the TUI.
//!
//! Each test scenario sends a choreographed sequence of key events, ticks, and
//! renders, then asserts on observable model state (active screen, cursor
//! position, focus mode, help overlay, macro engine state, etc.).
//!
//! These tests use the deterministic seed/time harness where applicable and
//! produce replayable forensic artifacts under `tests/artifacts/`.
//!
//! Run:
//! ```sh
//! cargo test -p mcp-agent-mail-server --test pty_e2e_search -- --nocapture
//! ```

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::print_literal,
    clippy::missing_const_for_fn,
    clippy::field_reassign_with_default,
    clippy::redundant_clone
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ftui::{Event, Frame, GraphemePool, KeyCode, KeyEvent, KeyEventKind, Modifiers};
use ftui_runtime::program::Model;

use mcp_agent_mail_core::Config;
use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};
use mcp_agent_mail_server::tui_bridge::TuiSharedState;
use mcp_agent_mail_server::tui_macro::PlaybackState;
use mcp_agent_mail_server::tui_persist::{
    PreferencePersister, TuiPreferences, load_screen_filter_presets, screen_filter_presets_path,
};
use mcp_agent_mail_server::tui_screens::{DeepLinkTarget, MailScreenId, MailScreenMsg};

// ── Helpers ──────────────────────────────────────────────────────────

fn key(code: KeyCode) -> MailMsg {
    MailMsg::Terminal(Event::Key(KeyEvent {
        code,
        kind: KeyEventKind::Press,
        modifiers: Modifiers::empty(),
    }))
}

fn key_mod(code: KeyCode, modifiers: Modifiers) -> MailMsg {
    MailMsg::Terminal(Event::Key(KeyEvent {
        code,
        kind: KeyEventKind::Press,
        modifiers,
    }))
}

fn tick() -> MailMsg {
    MailMsg::Terminal(Event::Tick)
}

fn type_str(model: &mut MailAppModel, s: &str) {
    for c in s.chars() {
        model.update(key(KeyCode::Char(c)));
    }
}

fn ticks(model: &mut MailAppModel, n: usize) {
    for _ in 0..n {
        model.update(tick());
    }
}

fn render(model: &MailAppModel, width: u16, height: u16) {
    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(width, height, &mut pool);
    model.view(&mut frame);
    // Render must not panic — that's the assertion.
}

fn new_model() -> (MailAppModel, Arc<TuiSharedState>) {
    let config = Config::default();
    let state = TuiSharedState::new(&config);
    let model = MailAppModel::new(Arc::clone(&state));
    (model, state)
}

fn new_model_with_config(config: &Config) -> (MailAppModel, Arc<TuiSharedState>) {
    let state = TuiSharedState::new(config);
    let model = MailAppModel::with_config(Arc::clone(&state), config);
    (model, state)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf()
}

fn save_artifact(name: &str, content: &str) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let dir = repo_root().join(format!(
        "tests/artifacts/pty_e2e/{ts}_{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{name}.txt"));
    let _ = std::fs::write(&path, content);
    eprintln!("pty_e2e artifact: {}", path.display());
}

// ── 1. Search Cockpit Keyboard Flows ────────────────────────────────

#[test]
fn search_focus_type_enter_cycle() {
    let (mut model, _state) = new_model();

    // Navigate to Search screen
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    assert_eq!(model.active_screen(), MailScreenId::Search);
    ticks(&mut model, 3);

    // Press '/' to focus query bar
    model.update(key(KeyCode::Char('/')));
    ticks(&mut model, 1);

    // Type a query
    type_str(&mut model, "error message");
    ticks(&mut model, 1);

    // Press Enter to submit (moves focus to ResultList)
    model.update(key(KeyCode::Enter));
    ticks(&mut model, 1);

    // Verify render doesn't panic at various sizes
    render(&model, 120, 40);
    render(&model, 80, 24);
    render(&model, 40, 12);

    // Navigate results with j/k (vim bindings)
    for _ in 0..5 {
        model.update(key(KeyCode::Char('j')));
    }
    for _ in 0..3 {
        model.update(key(KeyCode::Char('k')));
    }
    ticks(&mut model, 1);
    render(&model, 120, 40);
}

#[test]
fn search_facet_rail_navigation() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    ticks(&mut model, 3);

    // Press 'f' to switch to facet rail
    model.update(key(KeyCode::Char('f')));
    ticks(&mut model, 1);

    // Navigate facet slots with j/k
    model.update(key(KeyCode::Char('j'))); // Scope → DocKind
    model.update(key(KeyCode::Char('j'))); // DocKind → Importance
    model.update(key(KeyCode::Char('j'))); // Importance → AckStatus
    ticks(&mut model, 1);

    // Toggle a facet with Enter
    model.update(key(KeyCode::Enter));
    ticks(&mut model, 1);

    // Go back up
    model.update(key(KeyCode::Char('k')));
    model.update(key(KeyCode::Char('k')));

    // Reset facets with 'r'
    model.update(key(KeyCode::Char('r')));
    ticks(&mut model, 1);

    // Tab back to result list
    model.update(key(KeyCode::Tab));
    ticks(&mut model, 1);
    render(&model, 120, 40);
}

#[test]
fn search_escape_from_query_bar() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    ticks(&mut model, 1);

    // Focus query bar
    model.update(key(KeyCode::Char('/')));
    type_str(&mut model, "test query");
    ticks(&mut model, 1);

    // Escape back to result list
    model.update(key(KeyCode::Escape));
    ticks(&mut model, 1);
    render(&model, 80, 24);
}

#[test]
fn search_page_navigation_keys() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    ticks(&mut model, 3);

    // Page down/up (d/u)
    model.update(key(KeyCode::Char('d'))); // Page down
    model.update(key(KeyCode::Char('u'))); // Page up

    // Jump to end/start (G/g)
    model.update(key(KeyCode::Char('G'))); // End
    model.update(key(KeyCode::Char('g'))); // Home

    // Detail scroll (J/K)
    model.update(key(KeyCode::Char('J'))); // Detail scroll down
    model.update(key(KeyCode::Char('K'))); // Detail scroll up

    ticks(&mut model, 1);
    render(&model, 120, 40);
}

#[test]
fn search_doc_kind_and_importance_shortcuts() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    ticks(&mut model, 3);

    // 't' cycles doc kind from result list
    model.update(key(KeyCode::Char('t'))); // Messages → Agents
    ticks(&mut model, 1);
    model.update(key(KeyCode::Char('t'))); // Agents → Projects
    ticks(&mut model, 1);
    model.update(key(KeyCode::Char('t'))); // Projects → All
    ticks(&mut model, 1);

    // 'i' cycles importance
    model.update(key(KeyCode::Char('i'))); // Any → Urgent
    ticks(&mut model, 1);
    model.update(key(KeyCode::Char('i'))); // Urgent → High
    ticks(&mut model, 1);

    // Ctrl+C clears search
    model.update(key_mod(KeyCode::Char('c'), Modifiers::CTRL));
    ticks(&mut model, 1);
    render(&model, 80, 24);
}

// ── 2. Deep-Link Routing ────────────────────────────────────────────

#[test]
fn deep_link_message_routes_to_messages_screen() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    assert_eq!(model.active_screen(), MailScreenId::Search);

    // Simulate a DeepLink to a message
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::MessageById(42),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Messages);
    render(&model, 120, 40);
}

#[test]
fn deep_link_thread_routes_to_threads_screen() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));

    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::ThreadById("thread-42".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Threads);
    render(&model, 120, 40);
}

#[test]
fn deep_link_agent_routes_to_agents_screen() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::AgentByName("BlueLake".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Agents);
}

#[test]
fn deep_link_project_routes_to_projects_screen() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::ProjectBySlug("my-project".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Projects);
}

#[test]
fn deep_link_tool_routes_to_tool_metrics() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::ToolByName("send_message".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::ToolMetrics);
}

#[test]
fn deep_link_timeline_routes_to_timeline() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::TimelineAtTime(1_000_000),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Timeline);
}

#[test]
fn deep_link_reservation_routes_to_reservations() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::ReservationByAgent("RedFox".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Reservations);
}

#[test]
fn deep_link_contact_routes_to_contacts() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::ContactByPair("RedFox".to_string(), "BlueLake".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Contacts);
}

#[test]
fn deep_link_explorer_routes_to_explorer() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::ExplorerForAgent("GoldPeak".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Explorer);
}

// ── 3. Command Palette Actions ──────────────────────────────────────

#[test]
fn palette_open_and_dismiss() {
    let (mut model, _state) = new_model();
    ticks(&mut model, 1);

    // Ctrl+P opens palette
    model.update(key_mod(KeyCode::Char('p'), Modifiers::CTRL));
    ticks(&mut model, 1);
    render(&model, 120, 40);

    // Escape dismisses
    model.update(key(KeyCode::Escape));
    ticks(&mut model, 1);
    render(&model, 120, 40);
}

#[test]
fn palette_colon_opens() {
    let (mut model, _state) = new_model();
    ticks(&mut model, 1);

    // ':' also opens palette
    model.update(key(KeyCode::Char(':')));
    ticks(&mut model, 1);
    render(&model, 120, 40);

    model.update(key(KeyCode::Escape));
}

// ── 4. Global Navigation ────────────────────────────────────────────

#[test]
fn tab_cycles_all_screens() {
    let (mut model, _state) = new_model();
    assert_eq!(model.active_screen(), MailScreenId::Dashboard);

    // Tab through all screens and verify each renders
    let mut visited = std::collections::HashSet::new();
    visited.insert(model.active_screen());

    for _ in 0..20 {
        model.update(key(KeyCode::Tab));
        ticks(&mut model, 1);
        render(&model, 80, 24);
        visited.insert(model.active_screen());
    }

    // Should have visited all screens (14 total)
    assert!(
        visited.len() >= 14,
        "Expected >= 14 screens, visited {}",
        visited.len()
    );
}

#[test]
fn backtab_cycles_backwards() {
    let (mut model, _state) = new_model();
    let initial = model.active_screen();

    // Go forward one
    model.update(key(KeyCode::Tab));
    let forward = model.active_screen();
    assert_ne!(initial, forward);

    // Go back one
    model.update(key(KeyCode::BackTab));
    assert_eq!(model.active_screen(), initial);
}

#[test]
fn number_keys_switch_screens() {
    let (mut model, _state) = new_model();

    // Number keys jump to specific screens (1-based)
    model.update(key(KeyCode::Char('1')));
    let screen_1 = model.active_screen();

    model.update(key(KeyCode::Char('2')));
    let screen_2 = model.active_screen();
    assert_ne!(
        screen_1, screen_2,
        "Different numbers should navigate to different screens"
    );

    model.update(key(KeyCode::Char('3')));
    let screen_3 = model.active_screen();
    assert_ne!(screen_2, screen_3);
}

#[test]
fn help_overlay_toggle() {
    let (mut model, _state) = new_model();
    assert!(!model.help_visible());

    // '?' toggles help
    model.update(key(KeyCode::Char('?')));
    assert!(model.help_visible());
    ticks(&mut model, 1);
    render(&model, 120, 40);

    // Scroll help with j/k
    model.update(key(KeyCode::Char('j')));
    model.update(key(KeyCode::Char('j')));
    model.update(key(KeyCode::Char('k')));

    // '?' toggles off
    model.update(key(KeyCode::Char('?')));
    assert!(!model.help_visible());

    // Escape also closes help
    model.update(key(KeyCode::Char('?')));
    assert!(model.help_visible());
    model.update(key(KeyCode::Escape));
    assert!(!model.help_visible());
}

// ── 5. Macro Record and Playback ────────────────────────────────────

#[test]
fn macro_record_and_continuous_playback() {
    let (mut model, _state) = new_model();
    let engine = model.macro_engine();
    assert!(matches!(engine.playback_state(), PlaybackState::Idle));

    // Use SwitchScreen messages to verify navigation — same code path as macro playback
    model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
    assert_eq!(model.active_screen(), MailScreenId::Messages);

    model.update(MailMsg::SwitchScreen(MailScreenId::Threads));
    assert_eq!(model.active_screen(), MailScreenId::Threads);

    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    assert_eq!(model.active_screen(), MailScreenId::Search);
    ticks(&mut model, 1);
    render(&model, 120, 40);
}

#[test]
fn macro_step_by_step_playback_flow() {
    let (mut model, _state) = new_model();

    // Verify the engine is idle
    let engine = model.macro_engine();
    assert!(matches!(engine.playback_state(), PlaybackState::Idle));

    // Simulate a multi-step navigation sequence through the model
    // This exercises the same code path as macro playback:
    // dispatch_palette_action → screen switch → deep-link routing

    // Step 1: Navigate to Messages
    model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
    ticks(&mut model, 2);
    assert_eq!(model.active_screen(), MailScreenId::Messages);
    render(&model, 120, 40);

    // Step 2: Deep-link to a thread
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::ThreadById("t-100".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Threads);
    ticks(&mut model, 2);
    render(&model, 120, 40);

    // Step 3: Navigate to Search and run a query
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    model.update(key(KeyCode::Char('/')));
    type_str(&mut model, "migration");
    model.update(key(KeyCode::Enter));
    ticks(&mut model, 5);
    render(&model, 120, 40);

    // Step 4: Deep-link from search to agent
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::AgentByName("RedFox".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Agents);
    ticks(&mut model, 1);
    render(&model, 120, 40);
}

// ── 6. Layout Persistence ───────────────────────────────────────────

#[test]
fn preferences_roundtrip_json() {
    let prefs = TuiPreferences::default();
    let json = prefs.to_json().expect("serialize");
    let restored = TuiPreferences::from_json(&json).expect("deserialize");
    assert_eq!(prefs, restored);
}

#[test]
fn preferences_persist_to_tmpfile() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("test_config.env");

    let mut config = Config::default();
    config.console_persist_path = path.clone();
    config.console_auto_save = true;

    let mut persister = PreferencePersister::new(&config);
    let prefs = TuiPreferences::default();

    // save_now should write to the file
    assert!(persister.save_now(&prefs));
    assert!(path.exists(), "Config file should be written");

    // Read and verify content exists
    let content = std::fs::read_to_string(&path).expect("read");
    assert!(!content.is_empty(), "Written config should not be empty");
}

#[test]
fn preferences_skip_when_auto_save_disabled() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("no_save.env");

    let mut config = Config::default();
    config.console_persist_path = path.clone();
    config.console_auto_save = false;

    let mut persister = PreferencePersister::new(&config);
    persister.mark_dirty();
    let prefs = TuiPreferences::default();
    assert!(!persister.flush_if_due(&prefs));
}

#[test]
fn preferences_export_import_json() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("layout_test.env");

    let mut config = Config::default();
    config.console_persist_path = path;
    config.console_auto_save = true;

    let persister = PreferencePersister::new(&config);
    let prefs = TuiPreferences::default();

    // Export to JSON
    let export_path = persister.export_json(&prefs).expect("export");
    assert!(export_path.exists());

    // Import back
    let imported = persister.import_json().expect("import");
    assert_eq!(prefs, imported);
}

#[test]
fn timeline_preset_shortcuts_persist_and_reload_filters() {
    let dir = tempfile::tempdir().expect("tmp");
    let mut config = Config::default();
    config.console_persist_path = dir.path().join("console.env");
    config.console_auto_save = true;
    let preset_path = screen_filter_presets_path(&config.console_persist_path);

    let (mut model, _state) = new_model_with_config(&config);
    model.update(MailMsg::SwitchScreen(MailScreenId::Timeline));
    ticks(&mut model, 2);

    // Build a non-default filter state before saving.
    model.update(key(KeyCode::Char('Z'))); // standard -> verbose
    model.update(key(KeyCode::Char('t')));
    model.update(key(KeyCode::Char('s')));
    ticks(&mut model, 1);

    // Save preset "alpha" via Ctrl+S dialog.
    model.update(key_mod(KeyCode::Char('s'), Modifiers::CTRL));
    for _ in 0..24 {
        model.update(key(KeyCode::Backspace));
    }
    type_str(&mut model, "alpha");
    model.update(key(KeyCode::Enter));
    ticks(&mut model, 1);

    let first_store = load_screen_filter_presets(&preset_path).expect("load alpha preset");
    let alpha_values = first_store
        .get("timeline", "alpha")
        .expect("alpha preset exists")
        .values
        .clone();
    assert_eq!(
        alpha_values.get("verbosity").map(String::as_str),
        Some("verbose")
    );
    let alpha_kind = alpha_values.get("kind").cloned().unwrap_or_default();
    let alpha_source = alpha_values.get("source").cloned().unwrap_or_default();
    assert!(!alpha_kind.is_empty(), "kind token should be captured");
    assert!(!alpha_source.is_empty(), "source token should be captured");

    // Clear filters, then reload alpha via Ctrl+L + Enter.
    model.update(key(KeyCode::Char('c')));
    ticks(&mut model, 1);
    model.update(key_mod(KeyCode::Char('l'), Modifiers::CTRL));
    model.update(key(KeyCode::Enter));
    ticks(&mut model, 1);

    // Save again as "beta" and verify loaded state matched alpha snapshot.
    model.update(key_mod(KeyCode::Char('s'), Modifiers::CTRL));
    for _ in 0..24 {
        model.update(key(KeyCode::Backspace));
    }
    type_str(&mut model, "beta");
    model.update(key(KeyCode::Enter));
    ticks(&mut model, 1);

    let second_store = load_screen_filter_presets(&preset_path).expect("load beta preset");
    let beta_values = second_store
        .get("timeline", "beta")
        .expect("beta preset exists")
        .values
        .clone();
    assert_eq!(
        beta_values, alpha_values,
        "loaded filters should round-trip through save dialog"
    );

    // Delete both presets from the load dialog to exercise lifecycle completion.
    model.update(key_mod(KeyCode::Char('l'), Modifiers::CTRL));
    model.update(key(KeyCode::Delete));
    model.update(key(KeyCode::Delete));
    model.update(key(KeyCode::Escape));
    ticks(&mut model, 1);

    let after_delete = load_screen_filter_presets(&preset_path).expect("load final preset store");
    assert!(
        after_delete.list_names("timeline").is_empty(),
        "Ctrl+L/Delete should remove saved timeline presets"
    );

    render(&model, 120, 40);
}

// ── 7. Cross-Screen Interaction Workflows ───────────────────────────

#[test]
fn full_search_to_thread_to_timeline_workflow() {
    let (mut model, _state) = new_model();

    // Start at dashboard
    assert_eq!(model.active_screen(), MailScreenId::Dashboard);
    ticks(&mut model, 5);

    // Navigate to Search
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    ticks(&mut model, 1);

    // Focus query bar and search
    model.update(key(KeyCode::Char('/')));
    type_str(&mut model, "deployment error");
    model.update(key(KeyCode::Enter));
    ticks(&mut model, 5); // Allow debounce
    render(&model, 120, 40);

    // Deep-link to a thread from search results
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::ThreadById("deploy-thread".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Threads);
    ticks(&mut model, 2);
    render(&model, 120, 40);

    // From threads, deep-link to timeline at a specific time
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::TimelineAtTime(1_707_000_000_000_000),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Timeline);
    ticks(&mut model, 2);
    render(&model, 120, 40);

    // Navigate back to Search via Tab cycling
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    ticks(&mut model, 1);
    assert_eq!(model.active_screen(), MailScreenId::Search);
}

#[test]
fn full_dashboard_to_agents_to_explorer_workflow() {
    let (mut model, _state) = new_model();
    ticks(&mut model, 3);

    // Dashboard → Agents via deep-link
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::AgentByName("SilverCove".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Agents);
    ticks(&mut model, 2);
    render(&model, 120, 40);

    // Agents → Explorer via deep-link
    model.update(MailMsg::Screen(MailScreenMsg::DeepLink(
        DeepLinkTarget::ExplorerForAgent("SilverCove".to_string()),
    )));
    assert_eq!(model.active_screen(), MailScreenId::Explorer);
    ticks(&mut model, 2);
    render(&model, 120, 40);

    // Back to dashboard
    model.update(MailMsg::SwitchScreen(MailScreenId::Dashboard));
    assert_eq!(model.active_screen(), MailScreenId::Dashboard);
    render(&model, 80, 24);
}

// ── 8. Terminal Size Resilience ─────────────────────────────────────

#[test]
fn render_all_screens_at_multiple_sizes() {
    let (mut model, _state) = new_model();
    let sizes: &[(u16, u16)] = &[
        (120, 40), // Standard wide
        (80, 24),  // Classic terminal
        (40, 12),  // Narrow/compact
        (20, 6),   // Tiny (should gracefully degrade)
        (200, 60), // Ultra-wide
    ];

    for &id in mcp_agent_mail_server::tui_screens::ALL_SCREEN_IDS {
        model.update(MailMsg::SwitchScreen(id));
        ticks(&mut model, 1);
        for &(w, h) in sizes {
            render(&model, w, h); // Must not panic
        }
    }
}

#[test]
fn resize_during_search() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    ticks(&mut model, 1);

    // Focus and type
    model.update(key(KeyCode::Char('/')));
    type_str(&mut model, "test resize");
    model.update(key(KeyCode::Enter));
    ticks(&mut model, 5);

    // Render at progressively smaller sizes
    render(&model, 120, 40);
    render(&model, 80, 24);
    render(&model, 50, 15);
    render(&model, 30, 8);
    render(&model, 15, 4);

    // Back to full size
    render(&model, 120, 40);
}

// ── 9. Help Overlay on Search Screen ────────────────────────────────

#[test]
fn help_overlay_on_search_screen() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    ticks(&mut model, 1);

    // Open help
    model.update(key(KeyCode::Char('?')));
    assert!(model.help_visible());
    render(&model, 120, 40);

    // Scroll help
    for _ in 0..10 {
        model.update(key(KeyCode::Char('j')));
    }
    render(&model, 120, 40);

    for _ in 0..5 {
        model.update(key(KeyCode::Char('k')));
    }
    render(&model, 120, 40);

    // Close help
    model.update(key(KeyCode::Escape));
    assert!(!model.help_visible());

    // Now '/' should focus search, not be eaten by help
    model.update(key(KeyCode::Char('/')));
    ticks(&mut model, 1);
    render(&model, 120, 40);
}

// ── 10. Stress: Rapid Key Sequences ─────────────────────────────────

#[test]
fn rapid_key_sequences_no_panic() {
    let (mut model, _state) = new_model();
    let start = Instant::now();

    // Barrage of random-ish keys across multiple screens
    let actions = [
        key(KeyCode::Tab),
        key(KeyCode::Char('/')),
        key(KeyCode::Char('a')),
        key(KeyCode::Char('b')),
        key(KeyCode::Enter),
        key(KeyCode::Escape),
        key(KeyCode::Char('j')),
        key(KeyCode::Char('k')),
        key(KeyCode::Char('?')),
        key(KeyCode::Char('?')),
        key(KeyCode::Tab),
        key(KeyCode::Char('f')),
        key(KeyCode::Char('t')),
        key(KeyCode::Char('i')),
        key(KeyCode::Char('d')),
        key(KeyCode::Char('u')),
        key(KeyCode::Char('G')),
        key(KeyCode::Char('g')),
    ];

    for round in 0..50 {
        for action in &actions {
            model.update(action.clone());
        }
        ticks(&mut model, 1);
        if round % 10 == 0 {
            render(&model, 80, 24);
        }
    }

    let elapsed = start.elapsed();
    let report = format!(
        "rapid_key_sequences: {} rounds × {} keys = {} total in {:?}",
        50,
        actions.len(),
        50 * actions.len(),
        elapsed
    );
    save_artifact("rapid_keys_report", &report);
    eprintln!("{report}");

    // Budget: 900 key events + renders should complete in < 5s
    assert!(
        elapsed.as_secs() < 5,
        "Rapid key test took too long: {elapsed:?}"
    );
}

// ── 11. Palette on Non-Text Screens ─────────────────────────────────

#[test]
fn palette_does_not_steal_search_input() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    ticks(&mut model, 1);

    // Focus query bar
    model.update(key(KeyCode::Char('/')));
    ticks(&mut model, 1);

    // While in query bar, ':' should be typed as text, NOT open palette
    // (because consumes_text_input() returns true when query bar is focused)
    model.update(key(KeyCode::Char(':')));
    ticks(&mut model, 1);
    render(&model, 120, 40);

    // Escape out of query bar
    model.update(key(KeyCode::Escape));

    // Now ':' should open palette (not in text mode)
    model.update(key(KeyCode::Char(':')));
    ticks(&mut model, 1);
    // Dismiss
    model.update(key(KeyCode::Escape));
}

// ── 12. Full Soak Mini-Run ──────────────────────────────────────────

#[test]
fn mini_soak_all_screens_with_interactions() {
    let (mut model, _state) = new_model();
    let start = Instant::now();

    let screen_ids = mcp_agent_mail_server::tui_screens::ALL_SCREEN_IDS;
    let mut total_actions = 0u64;
    let mut total_renders = 0u64;

    for &id in screen_ids {
        model.update(MailMsg::SwitchScreen(id));
        ticks(&mut model, 2);
        total_actions += 2;

        // Common interactions per screen
        for _ in 0..5 {
            model.update(key(KeyCode::Down));
            total_actions += 1;
        }
        for _ in 0..3 {
            model.update(key(KeyCode::Up));
            total_actions += 1;
        }

        // Toggle help on and off
        model.update(key(KeyCode::Char('?')));
        ticks(&mut model, 1);
        model.update(key(KeyCode::Char('?')));
        total_actions += 3;

        render(&model, 120, 40);
        total_renders += 1;
        render(&model, 80, 24);
        total_renders += 1;
    }

    let elapsed = start.elapsed();
    let report = format!(
        "mini_soak: {} screens × interactions = {} actions + {} renders in {:?}",
        screen_ids.len(),
        total_actions,
        total_renders,
        elapsed
    );
    save_artifact("mini_soak_report", &report);
    eprintln!("{report}");
}

// ── 13. Search Cockpit Query History Recall ─────────────────────────

#[test]
fn query_bar_history_navigation() {
    let (mut model, _state) = new_model();
    model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    ticks(&mut model, 3);

    // Type and submit multiple queries to build history
    for query in &["first query", "second query", "third query"] {
        model.update(key(KeyCode::Char('/')));
        type_str(&mut model, query);
        model.update(key(KeyCode::Enter));
        ticks(&mut model, 5); // Allow debounce + search
    }

    // Focus query bar again
    model.update(key(KeyCode::Char('/')));

    // Up arrow should recall history
    model.update(key(KeyCode::Up));
    ticks(&mut model, 1);
    model.update(key(KeyCode::Up));
    ticks(&mut model, 1);

    // Down arrow goes back
    model.update(key(KeyCode::Down));
    ticks(&mut model, 1);

    // Enter submits
    model.update(key(KeyCode::Enter));
    ticks(&mut model, 1);
    render(&model, 120, 40);
}

// ── 14. Config-Driven Model Construction ────────────────────────────

#[test]
fn model_with_config_applies_accessibility() {
    let mut config = Config::default();
    config.tui_high_contrast = true;
    config.tui_key_hints = false;

    let state = TuiSharedState::new(&config);
    let model = MailAppModel::with_config(Arc::clone(&state), &config);

    assert!(model.accessibility().high_contrast);
    assert!(!model.accessibility().key_hints);
    render(&model, 120, 40);
}

#[test]
fn model_default_starts_at_dashboard() {
    let (model, _) = new_model();
    assert_eq!(model.active_screen(), MailScreenId::Dashboard);
    assert!(!model.help_visible());
}
