# Developer Guide: Extending AgentMailTUI

How to add screens, palette actions, keybindings, and tests to the
AgentMailTUI interactive operations console.

---

## Architecture Overview

The TUI runs on the main thread via `ftui` (a `ratatui`-based framework),
while the MCP server runs on a background thread via `asupersync`. They
communicate through `Arc<TuiSharedState>`, a lock-free shared state bridge.

```text
Main thread (ftui Program)          Background thread (asupersync)
┌──────────────────────┐            ┌────────────────────┐
│  MailAppModel        │◀──events──▶│  MCP HTTP Server    │
│  ├─ screens[7]       │            │  ├─ tool handlers   │
│  ├─ notifications    │            │  ├─ resource URIs   │
│  ├─ command palette  │            │  └─ storage/DB      │
│  └─ chrome (tabs,    │            └────────────────────┘
│      status, help)   │                     │
└──────────────────────┘                     │
         │                                   │
         └──────── Arc<TuiSharedState> ──────┘
                   (event ring, counters, stats)
```

Key files:

| File                      | Role                                    |
|---------------------------|-----------------------------------------|
| `tui_app.rs`              | `MailAppModel` — top-level app model     |
| `tui_chrome.rs`           | Tab bar, status line, help overlay       |
| `tui_bridge.rs`           | `TuiSharedState` — server ↔ TUI bridge  |
| `tui_events.rs`           | `MailEvent`, `EventSeverity`, ring buffer|
| `tui_theme.rs`            | Theme-aware style helpers                |
| `tui_keymap.rs`           | Global keybinding registry               |
| `tui_poller.rs`           | Background DB polling                    |
| `tui_screens/mod.rs`      | `MailScreen` trait, `MailScreenId` enum  |
| `tui_screens/dashboard.rs`| Dashboard screen implementation          |
| `tui_screens/messages.rs` | Message browser screen                   |
| `tui_screens/timeline.rs` | Timeline screen                          |
| etc.                      | One file per screen                      |

## Adding a New Screen

### Step 1: Define the screen ID

In `tui_screens/mod.rs`, add a variant to `MailScreenId`:

```rust
pub enum MailScreenId {
    Dashboard,
    Messages,
    Threads,
    Agents,
    Reservations,
    ToolMetrics,
    SystemHealth,
    MyNewScreen,  // <-- add here
}
```

Add it to `ALL_SCREEN_IDS`:

```rust
pub const ALL_SCREEN_IDS: &[MailScreenId] = &[
    // ... existing entries ...
    MailScreenId::MyNewScreen,
];
```

Update the screen registry in the same file with metadata (title, category,
description).

### Step 2: Implement `MailScreen`

Create `tui_screens/my_new_screen.rs`:

```rust
use ftui::{Event, Frame, KeyCode, Rect};
use crate::tui_bridge::TuiSharedState;
use crate::tui_screens::{Cmd, HelpEntry, MailScreen, MailScreenMsg};

pub struct MyNewScreen {
    // screen state
}

impl MyNewScreen {
    pub fn new() -> Self {
        Self { /* ... */ }
    }
}

impl MailScreen for MyNewScreen {
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('r') => {
                    // refresh data
                }
                _ => {}
            }
        }
        Cmd::none()
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        // render using ftui widgets
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry { key: "r", action: "Refresh" },
        ]
    }

    fn title(&self) -> &'static str {
        "My Screen"
    }

    fn tab_label(&self) -> &'static str {
        "MyScreen"
    }
}
```

### Step 3: Register in `tui_app.rs`

In `MailAppModel::new()`, add the screen to the screens vector:

```rust
screens.push(Box::new(my_new_screen::MyNewScreen::new()));
```

The screen index must match its position in `ALL_SCREEN_IDS`.

### Step 4: Export the module

In `tui_screens/mod.rs`:

```rust
pub mod my_new_screen;
```

### Step 5: Verify keybinding conflicts

The `tui_keymap.rs` test `no_screen_conflicts_with_global_bindings` will
automatically check that your screen's keybindings don't collide with global
keys (`q`, `?`, `:`, `m`, `T`, `1`-`8`). If a screen needs one of these
keys, set `consumes_text_input() -> true` for the input mode.

## Adding a Command Palette Action

### Step 1: Define an action ID

In `tui_app.rs`, add a constant:

```rust
pub mod palette_action_ids {
    pub const MY_ACTION: &str = "my:action";
}
```

### Step 2: Register the action

In the `build_palette_actions()` function:

```rust
out.push(
    ActionItem::new(palette_action_ids::MY_ACTION, "My Action")
        .with_description("Does something useful")
        .with_tags(&["keyword1", "keyword2"])
        .with_category("Category"),
);
```

### Step 3: Handle the action

In `MailAppModel::dispatch_palette_action()`:

```rust
palette_action_ids::MY_ACTION => {
    // your logic here
    true
}
```

## Adding a Global Keybinding

### Step 1: Add to `tui_keymap.rs`

In `GLOBAL_BINDINGS`:

```rust
GlobalBinding {
    label: "X",
    action: "My action",
    text_suppressible: true,  // suppressed when text input is active
},
```

### Step 2: Add handler in `tui_app.rs`

In the global key dispatch section of `MailAppModel::update()`:

```rust
KeyCode::Char('X') if !text_mode => {
    // your logic
    return Cmd::none();
}
```

### Step 3: Add to help overlay

In `tui_chrome.rs`, add to `GLOBAL_KEYBINDINGS`:

```rust
("X", "My action"),
```

### Step 4: Update the test

In `tui_keymap.rs`, add `"X"` to the `text_suppressible_flag_correctness`
test match arm.

## Theme-Aware Styling

Use `TuiThemePalette` for all colors:

```rust
use crate::tui_theme::TuiThemePalette;

fn render(&self, frame: &mut Frame, area: Rect) {
    let pal = TuiThemePalette::current();
    let header_style = Style::default().fg(pal.table_header_fg);
    let alt_bg = Style::default().bg(pal.table_row_alt_bg);
    // ...
}
```

Style helpers for common patterns:

```rust
use crate::tui_theme::{
    style_for_status,        // HTTP status codes (2xx green, 4xx yellow, 5xx red)
    style_for_latency,       // Latency gradient (<50ms green, <200ms yellow, red)
    style_for_agent_recency, // Agent activity (<60s green, <10min yellow, red)
    style_for_ttl,           // TTL countdown (>10min green, >1min yellow, red+bold)
    style_for_event_kind,    // MailEventKind colors
};
```

## Accessing Shared State

The `TuiSharedState` bridge provides:

```rust
// Read the latest snapshot (updated by tui_poller)
let snapshot = state.latest_snapshot();

// Access the event ring buffer
let events = state.events();

// Send a control message to the server
state.try_send_server_control(ServerControlMsg::ToggleTransportBase);
```

Data flows:
1. **Server → TUI**: Events pushed to `TuiSharedState` ring buffer
2. **Poller → TUI**: DB snapshots updated periodically by `tui_poller`
3. **TUI → Server**: Control messages via `try_send_server_control()`

## Testing

### Unit tests

Each screen should have tests in a `#[cfg(test)] mod tests` block within
its file. Test patterns:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ftui::{Event, KeyCode, KeyEvent};

    #[test]
    fn initial_state() {
        let screen = MyNewScreen::new();
        assert_eq!(screen.title(), "My Screen");
    }

    #[test]
    fn keybinding_refresh() {
        let mut screen = MyNewScreen::new();
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);

        let event = Event::Key(KeyEvent::new(KeyCode::Char('r')));
        let cmd = screen.update(&event, &state);
        // assert expected behavior
    }
}
```

### E2E tests

PTY-based interaction tests live in `tests/e2e/test_tui_interaction.sh`.
These use `expect`-style patterns to verify the TUI responds to keypresses.

To add a new E2E test:

```bash
test_my_feature() {
    local name="my_feature_test"
    start_tui_session "$name"

    send_key "$name" "8"         # press key 8 (new screen)
    sleep 0.3
    capture_screen "$name"

    local output
    output=$(cat "$SCRATCH/$name.screen")

    assert_contains "$output" "My Screen" "$name: screen title visible"
    assert_pass "$name"
}
```

### Keybinding conflict detection

The test `no_screen_conflicts_with_global_bindings` in `tui_keymap.rs`
automatically detects when a screen's keybindings conflict with global
text-suppressible keys. Run it after adding any keybindings:

```bash
cargo test -p mcp-agent-mail-server --lib -- no_screen_conflicts
```

## Checklist for New Screens

- [ ] `MailScreenId` variant added
- [ ] Added to `ALL_SCREEN_IDS`
- [ ] Screen registry metadata filled in
- [ ] `MailScreen` trait implemented
- [ ] Registered in `MailAppModel::new()`
- [ ] Module exported in `tui_screens/mod.rs`
- [ ] Keybindings defined (and verified no conflicts)
- [ ] Theme-aware colors used (no hardcoded `PackedRgba`)
- [ ] Unit tests written
- [ ] E2E test added for key interactions
- [ ] Help overlay entry for screen-specific bindings

## Extension Boundaries (Widgets/Search/Rendering)

Use this as the contributor contract for `br-3vwi.11.3`-class changes.

| Surface | Primary files | Do not bypass |
|---------|---------------|---------------|
| Widgets + screen composition | `crates/mcp-agent-mail-server/src/tui_widgets.rs`, `crates/mcp-agent-mail-server/src/tui_screens/*.rs`, `crates/mcp-agent-mail-server/src/tui_layout.rs` | Screen registry, keybinding conflict checks, theme palette usage |
| Search query behavior | `crates/mcp-agent-mail-db/src/search_planner.rs`, `crates/mcp-agent-mail-db/src/search_service.rs`, `crates/mcp-agent-mail-db/src/search_scope.rs` | `sanitize_fts_query` path, scope policy enforcement, search audit semantics |
| Markdown rendering/sanitization | `crates/mcp-agent-mail-server/src/markdown.rs`, `crates/mcp-agent-mail-server/src/tui_markdown.rs`, `crates/mcp-agent-mail-server/src/mail_ui.rs` | `render_markdown_to_safe_html` sanitizer flow and allowed-tag constraints |
| Artifact schema + replay evidence | `scripts/e2e_lib.sh`, `docs/SPEC-artifacts-bundle-schema.md` | `bundle.json`/`summary.json`/`repro.*` contract and schema validator |

## Non-Negotiable Invariants

### 1) Query-dialect parity

- Preserve existing search query semantics (`keyword`, `"phrase"`, `prefix*`, `AND/OR`, empty query behavior).
- Keep malformed/hostile query handling deterministic and non-crashing.
- Validate with:
  - `tests/e2e/test_search_cockpit.sh`
  - web parity checks in `tests/e2e/test_mail_ui.sh`

### 2) Permission + redaction safety

- Never expose blocked cross-project data in raw or preview forms.
- Keep deny/redact audit semantics consistent with scope policy.
- Validate with:
  - `cargo test -p mcp-agent-mail-db --test scope_policy_property -- --nocapture`
  - `tests/e2e/test_security_privacy.sh`

### 3) Markdown sanitization invariants

- All web-markdown rendering must go through `render_markdown_to_safe_html`.
- Script/style/event-handler vectors must be stripped; allowed styling must remain constrained.
- Validate with:
  - `cargo test -p mcp-agent-mail-server markdown -- --nocapture`
  - hostile markdown assertions in `tests/e2e/test_security_privacy.sh`

### 4) Artifact-schema compliance

- Every new/changed E2E flow must emit schema-valid bundle artifacts with replay metadata.
- Do not introduce ad-hoc artifact formats that bypass `bundle.json` indexing.
- Validate with:
  - `tests/e2e/test_artifacts_schema.sh`
  - `source scripts/e2e_lib.sh && e2e_validate_bundle_tree tests/artifacts`

## Required Validation Matrix Before Merge

| Change type | Required commands | Evidence artifacts to link in bead/comment |
|-------------|-------------------|--------------------------------------------|
| Search behavior/query dialect | `bash tests/e2e/test_search_cockpit.sh` | `tests/artifacts/search_cockpit/<timestamp>/case_*.txt`, `bundle.json` |
| Scope/redaction policy | `cargo test -p mcp-agent-mail-db --test scope_policy_property -- --nocapture` and `bash tests/e2e/test_security_privacy.sh` | `tests/artifacts/security_privacy/<timestamp>/case_01_search_scope.txt`, `case_09_secret_body.txt` |
| Search V3 CI contract (mandatory suites + diagnostics) | `bash tests/e2e/test_search_v3_stdio.sh`, `bash tests/e2e/test_search_v3_http.sh`, `bash tests/e2e/test_search_v3_shadow_parity.sh`, `bash tests/e2e/test_search_v3_resilience.sh`, `bash tests/e2e/test_search_v3_load_concurrency.sh`, plus `cargo test -p mcp-agent-mail-db --test filter_pagination --test logging_redaction --test diversity_dedup --test timeout_backpressure -- --nocapture` | `tests/artifacts/search_v3_*/<timestamp>/summary.json`, `tests/artifacts/search_v3/<suite>/<timestamp>/...`, `tests/artifacts/search_v3_ci/*.log` |
| Widget/screen interaction changes | `bash tests/e2e/test_tui_interactions.sh` and `bash tests/e2e/test_tui_compat_matrix.sh` | `tests/artifacts/tui_interactions/<timestamp>/trace/analytics_widgets_timeline.tsv`, matrix profile captures |
| Markdown rendering/sanitization | `cargo test -p mcp-agent-mail-server markdown -- --nocapture` and `bash tests/e2e/test_mail_ui.sh` | mail UI and security/privacy artifacts showing sanitized output |
| E2E harness/artifact format changes | `bash tests/e2e/test_artifacts_schema.sh` | `tests/artifacts/artifacts_schema/<timestamp>/bundle.json` |

### Search V3 CI Rerun Recipe (Verbose Diagnostics)

Use this to reproduce the CI Search V3 contract locally with deterministic clocks and artifact colocation:

```bash
export SEARCH_V3_LOG_ROOT="$PWD/tests/artifacts/search_v3"
export SV3_ARTIFACT_ROOT="$PWD/tests/artifacts/search_v3"
export E2E_CLOCK_MODE=deterministic
export E2E_SEED=424242

bash tests/e2e/test_search_v3_stdio.sh
bash tests/e2e/test_search_v3_http.sh
bash tests/e2e/test_search_v3_shadow_parity.sh
bash tests/e2e/test_search_v3_resilience.sh
bash tests/e2e/test_search_v3_load_concurrency.sh

cargo test -p mcp-agent-mail-db --test filter_pagination -- --nocapture
cargo test -p mcp-agent-mail-db --test logging_redaction -- --nocapture
cargo test -p mcp-agent-mail-db --test diversity_dedup -- --nocapture
cargo test -p mcp-agent-mail-db --test timeout_backpressure -- --nocapture

source scripts/e2e_lib.sh
e2e_validate_bundle_tree tests/artifacts
```

Minimum assertion floors enforced in CI:
- `search_v3_stdio >= 80`
- `search_v3_http >= 90`
- `search_v3_resilience >= 70`
- `search_v3_load_concurrency >= 60`

## Contributor Workflow for Extension Changes

1. Identify which surface(s) your change touches using the boundary table above.
2. Implement only within the owning module path; avoid parallel logic branches that skip shared policy/sanitizer/planner code.
3. Run the matching validation matrix entries and keep the generated artifact roots.
4. Record evidence in the bead comment: command, pass/fail counts, and artifact paths.
5. Only then close the bead or request review.
