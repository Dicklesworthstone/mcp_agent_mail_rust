# Conformance Audit 2026-04-18

Bead: `br-a2k3h.1`

Scope:
- Live tool inventory from `mcp_agent_mail_tools::TOOL_CLUSTER_MAP`
- Live resource registry from `mcp-agent-mail-server`
- Python behavior fixture at `crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json`
- Python tool description fixture at `tests/conformance/fixtures/tool_descriptions.json`
- Rust-native golden fixtures at `crates/mcp-agent-mail-conformance/tests/conformance/fixtures/rust_native/`
- Rust-native tool-filter fixture at `crates/mcp-agent-mail-conformance/tests/conformance/fixtures/tool_filter/cases.json`
- Direct source inspection in `crates/mcp-agent-mail-tools/src/resources.rs`

Headline counts:
- Tools: 37 total = 34 python-parity + 3 rust-native fixture-backed
- Resources: 25 logical templates = 23 python-parity + 2 rust-native uncovered (`resource://tooling/metrics_core`, `resource://tooling/diagnostics`)
- Current suite state: the pre-`3813da8f` full-suite audit still records failures in `tests/conformance.rs`, and the dedicated Rust-native fixture lane now exists. A targeted `rch` verification attempt on 2026-04-18T09:59Z did not reach assertions because the remote worker ran out of disk space while compiling (`No space left on device`).

## Tool coverage table

| tool_name | has_fixture | passes | classification (python-parity / rust-native / unknown) | fixture_file | notes |
| --- | --- | --- | --- | --- | --- |
| health_check | yes | no | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case in the Python behavior fixture, but the current run fails because `health_check` now returns `status=error` plus semantic-readiness/recovery fields when the DB is absent. |
| ensure_project | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| install_precommit_guard | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| uninstall_precommit_guard | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| register_agent | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| create_agent_identity | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| whois | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| resolve_pane_identity | yes | pending | rust-native | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/rust_native/resolve_pane_identity.json | Dedicated Rust-native golden fixture added in `3813da8f`; latest targeted `rch` verification was blocked by remote worker disk exhaustion before test execution. |
| cleanup_pane_identities | yes | pending | rust-native | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/rust_native/cleanup_pane_identities.json | Dedicated Rust-native golden fixture added in `3813da8f`; latest targeted `rch` verification was blocked by remote worker disk exhaustion before test execution. |
| list_agents | yes | pending | rust-native | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/rust_native/list_agents.json | Previously uncovered; now covered by the dedicated Rust-native fixture lane added in `3813da8f`. Latest targeted verification was blocked by remote worker disk exhaustion before test execution. |
| send_message | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 4 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| reply_message | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 3 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| fetch_inbox | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| mark_message_read | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| acknowledge_message | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| request_contact | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| respond_contact | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| list_contacts | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| set_contact_policy | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| file_reservation_paths | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| release_file_reservations | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| renew_file_reservations | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 3 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| force_release_file_reservation | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| search_messages | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 3 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| summarize_thread | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 5 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| macro_start_session | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| macro_prepare_thread | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| macro_file_reservation_cycle | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| macro_contact_handshake | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| ensure_product | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| products_link | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| search_messages_product | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| fetch_inbox_product | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| summarize_thread_product | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| acquire_build_slot | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| renew_build_slot | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |
| release_build_slot | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 case(s) in the Python behavior fixture; exercised by `run_fixtures_against_rust_server_router`. |

## Resource coverage table

| resource_template | has_fixture | passes | classification | fixture_file | notes |
| --- | --- | --- | --- | --- | --- |
| resource://config/environment | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 fixture URI(s): resource://config/environment; resource://config/environment?format=json |
| resource://identity/{project} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://identity/abs-path-backend |
| resource://agents/{project_key} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://agents/abs-path-backend |
| resource://tooling/directory | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 fixture URI(s): resource://tooling/directory; resource://tooling/directory?format=json |
| resource://tooling/schemas | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 fixture URI(s): resource://tooling/schemas; resource://tooling/schemas?format=json |
| resource://tooling/metrics | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 fixture URI(s): resource://tooling/metrics; resource://tooling/metrics?format=json |
| resource://tooling/metrics_core | no | no | rust-native | none | Registered in Rust and unit-tested in `mcp-agent-mail-tools/src/resources.rs`, but missing conformance fixture coverage. Follow-up: `br-a2k3h.4` and `br-a2k3h.6`. |
| resource://tooling/diagnostics | no | no | rust-native | none | Registered in Rust and unit-tested in `mcp-agent-mail-tools/src/resources.rs`, but missing conformance fixture coverage. Follow-up: `br-a2k3h.4` and `br-a2k3h.6`. |
| resource://tooling/locks | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 fixture URI(s): resource://tooling/locks; resource://tooling/locks?format=json |
| resource://tooling/capabilities/{agent} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://tooling/capabilities/BlueLake?project=abs-path-backend |
| resource://tooling/recent/{window_seconds} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://tooling/recent/60?agent=BlueLake&project=abs-path-backend |
| resource://projects | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 2 fixture URI(s): resource://projects; resource://projects?format=json |
| resource://project/{slug} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://project/abs-path-backend |
| resource://product/{key} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://product/0123456789abcdef0123 |
| resource://message/{message_id} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://message/2?project=abs-path-backend |
| resource://thread/{thread_id} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://thread/2?project=abs-path-backend&include_bodies=true |
| resource://inbox/{agent} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://inbox/GreenCastle?project=abs-path-backend&include_bodies=true&limit=10 |
| resource://mailbox/{agent} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://mailbox/GreenCastle?project=abs-path-backend&limit=10 |
| resource://mailbox-with-commits/{agent} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://mailbox-with-commits/GreenCastle?project=abs-path-backend&limit=10 |
| resource://outbox/{agent} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://outbox/BlueLake?project=abs-path-backend&limit=10&include_bodies=true |
| resource://views/urgent-unread/{agent} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://views/urgent-unread/GreenCastle?project=abs-path-backend&limit=10 |
| resource://views/ack-required/{agent} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://views/ack-required/GreenCastle?project=abs-path-backend&limit=10 |
| resource://views/acks-stale/{agent} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://views/acks-stale/GreenCastle?project=abs-path-backend&ttl_seconds=60&limit=10 |
| resource://views/ack-overdue/{agent} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://views/ack-overdue/GreenCastle?project=abs-path-backend&ttl_minutes=1&limit=10 |
| resource://file_reservations/{slug} | yes | yes | python-parity | crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json | 1 fixture URI(s): resource://file_reservations/abs-path-backend?active_only=false |

## Skip/ignore inventory

- `#[ignore]`: none found under `crates/mcp-agent-mail-conformance/tests/`
- `#[should_panic]`: none found under `crates/mcp-agent-mail-conformance/tests/`
- `#[cfg(...)]`: none found under `crates/mcp-agent-mail-conformance/tests/`
- The prior placeholder file `crates/mcp-agent-mail-conformance/tests/conformance_debug.rs` is now the audit cross-check test surface for this bead.

## Test-run output

Post-`3813da8f` note:
- Dedicated Rust-native fixture files and harness coverage now exist for `resolve_pane_identity`, `cleanup_pane_identities`, and `list_agents`.
- Targeted verification command attempted:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/target-$(whoami)-am cargo test -p mcp-agent-mail-conformance rust_native -- --nocapture
```

Latest result from remote worker `vmi1293453`:
- Exit status: `101`
- Failure mode: remote compile/link exhaustion, not an assertion mismatch
- Key diagnostic: `No space left on device (os error 28)`
- Impact: the Rust-native fixture lane remains present and audit-visible, but still needs a clean rerun on a worker with sufficient disk

The historical full-suite results below predate that refresh and should not be read as the current status of the Rust-native Identity coverage lane.

Command sequence run on 2026-04-18:

```bash
CARGO_TARGET_DIR=/tmp/target-$(whoami)-am rch exec -- cargo test -p mcp-agent-mail-conformance --test conformance_debug -- --nocapture
CARGO_TARGET_DIR=/tmp/target-$(whoami)-am rch exec -- cargo test -p mcp-agent-mail-conformance -- --nocapture
```

Audit cross-check test:
- Remote worker: `ts2`
- Exit status: `0`
- Duration: about `139.200s`
- Result: `tests/conformance_debug.rs` passed `2/2`

Full suite run:
- Remote worker: `ts2`
- Exit status: `101`
- Duration: about `146.664s`
- `src/lib.rs`: `0/0`
- `src/main.rs`: `3/3` passed
- `tests/conformance.rs`: `9/13` passed, `4` failed
- Cargo stopped after the failing `tests/conformance.rs` binary, so later integration test binaries were not reached in that invocation

Failures dissected:
- `fixture_schema_drift_guard`
  Root cause in the pre-`3813da8f` audit run: the guard only accepted Python behavior fixtures and did not understand the new Rust-native Identity coverage surface.
  Evidence at the time: `tool resolve_pane_identity is registered in TOOL_CLUSTER_MAP but has no fixture`
  Follow-up: `br-a2k3h.6`
- `run_fixtures_against_rust_server_router`
  Root cause: the `health_check` Python fixture expects legacy `status=ok` output, but the Rust server now returns `status=error` with semantic-readiness and recovery fields when the SQLite DB file is missing.
  Evidence: `tool health_check case default: output mismatch`
  Follow-up: explicit product decision still needed; not worth a separate bead until `br-a2k3h.7` or a future fixture-refresh task decides whether to preserve or regenerate the health baseline
- `tool_filter_profiles_match_fixtures`
  Root cause in the pre-`3813da8f` audit run: the actual tool list included `list_agents`, but `tests/conformance/fixtures/tool_filter/cases.json` did not.
  Evidence at the time: `tools/list mismatch for case disabled_full`
  Follow-up: `br-a2k3h.6`
- `toon_format_resolution_json_fallback`
  Root cause: same health-check drift as the main fixture run; the fallback assertion still expects `status=ok`.
  Evidence: `health_check must return status=ok`
  Follow-up: same decision path as `run_fixtures_against_rust_server_router`

Timing per test / slow outliers:
- Standard `cargo test -- --nocapture` did not emit per-test timings, so the nearest reliable timings are per-binary:
  - `tests/conformance.rs`: `26.60s`
  - `tests/conformance_debug.rs`: `0.01s`
  - `src/main.rs`: `0.03s`
- The end-to-end `rch` wall-clock for the successful audit cross-check run was `139.200s`; the full-suite rerun took `146.664s`
- Historical note: an earlier full-suite attempt in this same turn failed before the harness launched because `mcp-agent-mail-server` could not resolve `BROWSER_TUI_DEFERRED_JSON` / `BROWSER_TUI_DEFERRED_HTML`. That transient blocker was recorded under `br-0ijq8` and was no longer reproducible by the later reruns above.

## Mystery states

- `list_agents` is no longer an uncovered mystery state: `3813da8f` added dedicated Rust-native fixtures for it under `tests/conformance/fixtures/rust_native/`. Remaining follow-up is the drift-guard work in `br-a2k3h.6`.
- `resource://tooling/metrics_core` and `resource://tooling/diagnostics` are registered by the live router and have Rust unit tests in `mcp-agent-mail-tools/src/resources.rs:5114-5131`, but neither has behavior fixtures in the conformance crate. Follow-up: `br-a2k3h.4` and `br-a2k3h.6`.
- The current tool-description parity and drift-guard tests still need to be taught about the dedicated Rust-native Identity fixture lane. Follow-up: `br-a2k3h.6`.
- Earlier same-day crate-doc count drift was folded into the documentation-alignment sweep, so the shipped crate docs now match the live 37-tool / 25-resource surface.
- Not worth tracking as a separate bead: the apparent `tests/conformance/fixtures/python_reference.json` mismatch is only a package-root vs workspace-root path confusion. The tracked fixture is present where the package test binary expects it.
