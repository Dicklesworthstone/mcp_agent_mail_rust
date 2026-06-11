# Dependency Upgrade Log

**Date:** 2026-06-11 | **Project:** mcp_agent_mail_rust | **Language:** Rust

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

### 2026-06-11 local /dp refresh

- `asupersync`: `0.3.3` -> `0.3.4` to match `/dp/asupersync`.
- `beads_rust`: manifest constraint `0.2.10` -> `0.2.15` to match `/dp/beads_rust`.
- `franken-agent-detection`: `0.1.7` -> `0.1.8` to match `/dp/franken_agent_detection`.
- `frankensearch`: `0.3.0` -> `0.3.2` to match `/dp/frankensearch/frankensearch`.
- `frankensearch-core`, `frankensearch-embed`, `frankensearch-index`, `frankensearch-fusion`: `0.2.0` -> `0.2.1` to match the current local `/dp/frankensearch` crate versions.

### Compatible registry updates

- `tru`/`toon`: `0.2.2` -> `0.2.3`.
- `zip`: manifest floor tightened to `8.6.0` after lockfile resolution selected it.

### Manifest-level latest-stable updates

- `comrak`: `0.50.0` -> `0.52.0`.
- `crossterm`: `0.28.1` -> `0.29.0`.
- `getrandom`: `0.2.17` -> `0.4.2`; call sites now use `getrandom::fill`.
- `insta`: manifest floor tightened from `1.38` to the resolved current `1.47.2`.
- `json5`: `0.4.1` -> `1.3.1`.
- `plist`: `1.8.0` -> `1.9.0`.
- `sha1`: `0.10.6` -> `0.11.0`.
- `sha2`: `0.10.9` -> `0.11.0`.
- `similar`: `2.7.0` -> `3.1.0`.
- `tantivy`: `0.25.0` -> `0.26.1`.
- `tokenizers`: `0.22.2` -> `0.23.1`.
- `unicode-width`: `0.1.14` -> `0.2.2`.

### 2026-06-11 latest-stable direct registry refresh

- `git2`: `0.20.4` -> `0.21.0`; current source uses stable repository/status/diff APIs.
- `toml_edit`: `0.23.10+spec-1.0.0` -> `0.25.12+spec-1.1.0`; current source uses `DocumentMut`, `Item`, `Array`, and `value` APIs.
- `safetensors`: `0.7.0` -> `0.8.0`; no direct source call sites, optional dependency only.
- `wide`: `1.4.0` -> `1.5.0`; no direct source call sites, dependency surface only.

### 2026-06-11 compatible transitive refresh

- Ran `cargo update`, resolving 54 package changes including `chrono`, `dashmap`, `minijinja`, `regex`, `uuid`, `wasm-bindgen`, `zerocopy`, and related transitive crates to their latest compatible stable versions.

## Failed

- Pending.

## Needs Attention

- `cargo outdated` cannot inspect this workspace directly because it copies the manifest to a temporary directory where relative `/dp` patches like `../asupersync` resolve to missing paths such as `/tmp/asupersync`. I am using `cargo update --dry-run --verbose`, `cargo metadata`, and direct local manifest checks instead.
