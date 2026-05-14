# Dependency Upgrade Log

**Date:** 2026-05-14 | **Project:** mcp_agent_mail_rust | **Language:** Rust

## Summary

- **Updated:** in progress
- **Skipped:** in progress
- **Failed:** in progress
- **Needs attention:** in progress

## Updates

### Local /dp dependency alignment

- `fastmcp*`: `0.3.0` -> `0.3.1` to match `/dp/fastmcp_rust`.
- `sqlmodel*`: `0.2.1` -> `0.2.2` to match `/dp/sqlmodel_rust`.
- `ftui*`: `0.3.1` -> `0.4.0` to match `/dp/frankentui`.
- `beads_rust`: `0.2.6` -> `0.2.7` to match `/dp/beads_rust`.
- Removed the unused `sqlmodel-frankensqlite` patch entry because the active workspace graph does not depend on that package; keeping it caused Cargo patch warnings.

### Compatible registry updates

- Pending.

### Manifest-level latest-stable updates

- Pending.

## Failed

- Pending.

## Needs Attention

- `cargo outdated` cannot inspect this workspace directly because it copies the manifest to a temporary directory where relative `/dp` patches like `../asupersync` resolve to missing paths such as `/tmp/asupersync`. I am using `cargo update --dry-run --verbose`, `cargo metadata`, and direct local manifest checks instead.
