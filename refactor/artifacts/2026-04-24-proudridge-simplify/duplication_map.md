# Duplication Map — 2026-04-24-proudridge-simplify

Generated: 2026-04-24 22:21 UTC
Tools run: manual diff/code review plus local heuristic scan
Raw scan transcript: local-only generated artifact; durable findings summarized below.

| ID  | Kind | Locations | LOC each | × | Type | Notes |
|-----|------|-----------|----------|---|------|-------|
| D1  | Repeated lossy agent-name normalization | `crates/mcp-agent-mail-tools/src/messaging.rs`: `send_message`, `reply_message`, `fetch_inbox`, `mark_message_read`, `acknowledge_message` | 2-11 | 5 | II | Same expression: `normalize_agent_name(&name).unwrap_or(name)` and Vec mapping variants. Closed over owned `String` inputs; helper preserves invalid-name fallback. Score: LOC 2 * confidence 5 / risk 1 = 10.0. |
| R1  | Baseline formatting drift | `crates/mcp-agent-mail-cli/src/lib.rs`, `crates/mcp-agent-mail-db/src/reconstruct.rs` | n/a | n/a | n/a | `cargo fmt --check` failed before refactor edits. Normalized with `cargo fmt` after reserving exact files; tracked separately from D1. |
