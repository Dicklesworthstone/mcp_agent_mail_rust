- title: "Track 1: Server/TUI crate unit tests (~16 untested modules)"
  type: epic
  priority: P2
  parent: br-33ha

- title: "Unit tests for tui_app.rs state machine (screen transitions, action dispatch)"
  type: task
  priority: P2
  parent: br-33ha.1

- title: "Unit tests for tui_screens/ data transformation (all 9 screen modules)"
  type: task
  priority: P2
  parent: br-33ha.1

- title: "Unit tests for tui_events.rs + tui_keymap.rs (event routing, key binding resolution)"
  type: task
  priority: P2
  parent: br-33ha.1

- title: "Unit tests for tui_bridge.rs + tui_poller.rs (DB data fetching, poll scheduling)"
  type: task
  priority: P2
  parent: br-33ha.1

- title: "Unit tests for tui_persist.rs + tui_theme.rs + tui_chrome.rs"
  type: task
  priority: P2
  parent: br-33ha.1

- title: "Unit tests for startup_checks.rs (validation logic, error formatting)"
  type: task
  priority: P2
  parent: br-33ha.1

- title: "Unit tests for retention.rs + cleanup.rs + ack_ttl.rs (policy logic)"
  type: task
  priority: P2
  parent: br-33ha.1

- title: "Unit tests for disk_monitor.rs (threshold checks, alert logic)"
  type: task
  priority: P2
  parent: br-33ha.1

- title: "Unit tests for console.rs + mail_ui.rs + markdown.rs + templates.rs"
  type: task
  priority: P2
  parent: br-33ha.1

- title: "Unit tests for static_files.rs + theme.rs (MIME types, asset serving)"
  type: task
  priority: P2
  parent: br-33ha.1
