# Experimental Surfaces

This directory holds parked or deferred code that is intentionally kept out of
the active Cargo workspace.

Current contents:

- `mcp-agent-mail-wasm/`: the retired standalone WASM/browser client prototype
  parked by `br-il53l.12`

The goal is preservation, not shipment:

- these crates stay available as prior art if the project revisits the idea
- they are excluded from normal workspace build and release flows
- they may require `--manifest-path` when built directly

Example:

```bash
cargo check --manifest-path experimental/mcp-agent-mail-wasm/Cargo.toml
```
