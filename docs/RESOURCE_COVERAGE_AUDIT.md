# Resource Coverage Audit

Bead: `br-a2k3h.4`

Date: `2026-04-18`

## Scope

This audit walks the live MCP resource surface exposed by
`crates/mcp-agent-mail-tools/src/resources.rs` and compares it against the
fixture-backed coverage already recorded in
[`docs/CONFORMANCE_AUDIT_2026-04-18.md`](./CONFORMANCE_AUDIT_2026-04-18.md).

Coverage in this document means conformance-harness fixture coverage in
`mcp-agent-mail-conformance`, not just ad hoc unit tests in the resource
implementation.

## Summary

- The live Rust router exposes 25 logical resource templates after collapsing
  `?{query}` variants.
- 23 of the 25 templates are covered by Python-parity fixtures in
  `crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json`.
- 2 templates are Rust-only resources that are registered and unit-tested, but
  still lack dedicated conformance fixtures:
  - `resource://tooling/metrics_core`
  - `resource://tooling/diagnostics`

## Raw Registry Notes

- The raw registry in `crates/mcp-agent-mail-tools/src/resources.rs` includes
  both base and query-aware variants for several resources.
- These collapse to the following 25 logical templates for audit purposes:
  `config/environment`, `tooling/directory`, `tooling/schemas`,
  `tooling/metrics`, `tooling/metrics_core`, `tooling/diagnostics`,
  `tooling/locks`, and `projects` each ship both base and `?{query}` forms.

## Coverage Matrix

| template | cluster | raw_variants | fixture_status | evidence | gap |
| --- | --- | --- | --- | --- | --- |
| `resource://config/environment` | config | 2 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://identity/{project}` | identity | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://agents/{project_key}` | identity | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://tooling/directory` | tooling | 2 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://tooling/schemas` | tooling | 2 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://tooling/metrics` | tooling | 2 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://tooling/metrics_core` | tooling | 2 | gap | Registered in `resources.rs`; mentioned as uncovered in `CONFORMANCE_AUDIT_2026-04-18.md` | unit-tested only; no conformance fixture |
| `resource://tooling/diagnostics` | tooling | 2 | gap | Registered in `resources.rs`; mentioned as uncovered in `CONFORMANCE_AUDIT_2026-04-18.md` | unit-tested only; no conformance fixture |
| `resource://tooling/locks` | tooling | 2 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://tooling/capabilities/{agent}` | tooling | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://tooling/recent/{window_seconds}` | tooling | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://projects` | project directory | 2 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://project/{slug}` | project directory | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://product/{key}` | product bus | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://message/{message_id}` | mailbox | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://thread/{thread_id}` | mailbox | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://inbox/{agent}` | mailbox | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://mailbox/{agent}` | mailbox | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://mailbox-with-commits/{agent}` | mailbox | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://outbox/{agent}` | mailbox | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://views/urgent-unread/{agent}` | views | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://views/ack-required/{agent}` | views | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://views/acks-stale/{agent}` | views | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://views/ack-overdue/{agent}` | views | 1 | covered | Python-parity fixture in `python_reference.json` | none |
| `resource://file_reservations/{slug}` | reservations | 1 | covered | Python-parity fixture in `python_reference.json` | none |

## Gaps

The only fixture-coverage gaps in the logical resource surface are:

1. `resource://tooling/metrics_core`
2. `resource://tooling/diagnostics`

Both resources are real parts of the Rust router, but they currently stop at
unit-test coverage inside `mcp-agent-mail-tools`. They are not yet represented
in the conformance harness, so resource-surface drift would not fail the
conformance lane today.

## Next Actions

- Add dedicated Rust-native conformance coverage for
  `resource://tooling/metrics_core` and `resource://tooling/diagnostics`.
- Teach the drift guard in `br-a2k3h.6` to treat every logical resource
  template the same way it treats tools: each live template must have either a
  Python-parity fixture source or a Rust-native conformance source.
- After that lands, refresh
  [`docs/CONFORMANCE_AUDIT_2026-04-18.md`](./CONFORMANCE_AUDIT_2026-04-18.md)
  and the `mcp-agent-mail-conformance` crate README so they no longer describe
  these two resources as uncovered.
