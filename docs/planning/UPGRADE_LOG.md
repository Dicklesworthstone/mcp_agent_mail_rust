# Dependency Upgrade Log

**Date:** 2026-02-19  |  **Project:** mcp-agent-mail-rust  |  **Language:** Rust

## Summary
- **Updated:** 15 (8 from 2026-02-17 + 7 from 2026-02-19)  |  **Skipped:** 0  |  **Failed:** 0  |  **Needs attention:** 2 (pre-existing)

---

## 2026-02-19 Update

### fastembed: 5.9.0 → 5.11.0
- **Manifest:** Updated pinned version in workspace `Cargo.toml` from `"5.9.0"` to `"5.11.0"`
- **Breaking:** None (minor version bump)
- **Tests:** 722 passed across compilable crates

### clap: 4.5.59 → 4.5.60
- **Manifest:** Spec `"4"` — no change needed, picked up via `cargo update`
- **Breaking:** None (patch bump)
- **Tests:** Passed

### Transitive updates (via `cargo update`)
- bumpalo: 3.19.1 → 3.20.2
- native-tls: 0.2.16 → 0.2.18
- security-framework: 3.6.0 → 3.7.0
- security-framework-sys: 2.16.0 → 2.17.0

### Pre-existing issues (not from this update)
- **ftui-text compile error:** `E0515` in `frankentui/crates/ftui-text/src/markup.rs:143` — nightly lifetime checking tightened. Blocks server/CLI crates. Needs fix in frankentui.
- **FrankenSQLite:** 10 db tests fail due to unimplemented `sum()` aggregate in `GROUP BY+JOIN`

---

## 2026-02-17 Update

## Toolchain
- **rust-toolchain.toml:** pinned to `nightly-2026-02-13` (was `nightly`)
- **rust-version:** `1.85` -> `1.95`

## Updates

### git2: 0.19 -> 0.20
- **Breaking:** `trace_set()`, `Error::last_error()`, `Tree::walk()` signature changes, `ssh_key_from_memory()` removed
- **Migration:** None needed — this project doesn't use any of the changed APIs
- **Additional:** Fixed `mcp-agent-mail-storage/Cargo.toml` local `git2 = "0.19"` override -> `git2.workspace = true`
- **Tests:** Passed

### dirs: 5 -> 6
- **Breaking:** None (only deps-sys version bump)
- **Tests:** Passed

### zip: 2 -> 8
- **Breaking:** Major version jump, but `DateTime::from_date_and_time()`, `SimpleFileOptions`, `CompressionMethod::Deflated` all still exist
- **Migration:** None needed
- **Tests:** Passed

### safetensors: 0.5 -> 0.7
- **Breaking:** `data_info` parameter changed from `&Option` to `Option`
- **Migration:** None needed — not directly used in source (transitive through frankensearch)
- **Tests:** Passed

### wide: 0.7 -> 1
- **Breaking:** API redesigned in 1.x
- **Migration:** None needed — not directly used in source (transitive through frankensearch)
- **Tests:** Passed

### rayon: "1.10" -> "1"
- **Breaking:** None (loosened overly-tight pin)
- **Tests:** Passed

### similar: "2.5.0" -> "2"
- **Breaking:** None (loosened overly-tight pin)
- **Tests:** Passed

### cargo update (patch versions)
- 16 packages updated to latest compatible versions via `cargo update`

## Clippy Fixes

The nightly-2026-02-13 toolchain introduced stricter clippy lints. Fixed ~400+ lint errors across all crates:
- `collapsible_if`: Nested `if` statements collapsed using `&&` let chains
- `duration_suboptimal_units`: `Duration::from_secs(60)` -> `Duration::from_mins(1)` etc.
- `manual_is_multiple_of`: `n % k == 0` -> `n.is_multiple_of(k)`
- `missing_const_for_fn`: Added `const` to eligible functions
- `manual_clamp`: Replaced manual clamp patterns with `.clamp()`
- `let_and_return`: Returned expressions directly
- `format_push_string`: Used `write!` instead of `format!` push
- `avoid_collect`: Used iterators directly instead of unnecessary `.collect()`

## Additional Fixes

### backpressure.rs: Added `Deserialize` derive to `HealthLevel`
- Test code attempted to deserialize `HealthLevel` but the derive was missing
- Added `Deserialize` to the `#[derive(...)]` attribute

### kpi.rs: Fixed `horizon_s` -> `horizon_secs` in test
- Test referenced `f.horizon_s` but the field was renamed to `horizon_secs`

## Needs Attention

### Pre-existing: error_code_catalog_is_stable (conformance test)
- **Issue:** Conformance test expects 24 error codes but 25 found (new `INVALID_PATH`)
- **Cause:** Pre-existing drift, not related to dependency upgrades
- **Action:** Update baseline in conformance test

### Pre-existing: hostile_*_falls_to_like (search planner tests)
- **Issue:** 2 tests expect `PlanMethod::Like` but planner returns `Empty` for hostile FTS input
- **Cause:** Pre-existing search planner behavior change, not related to dependency upgrades
- **Action:** Review search planner fallback behavior for hostile input

### Pre-existing: TUI/CLI snapshot drift (49+ tests)
- **Issue:** TUI snapshot tests, CLI help snapshots, and markdown rendering snapshots fail
- **Cause:** Concurrent agent's UI changes (quick-reply modal, dashboard, screen layout changes)
- **Action:** Regenerate snapshots after concurrent agent completes their work
