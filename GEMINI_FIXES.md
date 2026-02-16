# Gemini Fixes Summary - 2026-02-16

## Critical Bug Fixes

### 1. Cross-Platform Absolute Path Handling in `ensure_project`
- **Component:** `crates/mcp-agent-mail-db`
- **Issue:** `ensure_project` path validation relied on `starts_with('/')`, which rejected valid Windows absolute paths (e.g., `C:\Projects\Backend`).
- **Fix:** Updated `crates/mcp-agent-mail-db/src/queries.rs` to use `std::path::Path::new(..).is_absolute()`, ensuring correct behavior on both Unix and Windows.

### 2. Configurable Absolute Attachment Paths in Bundle
- **Component:** `crates/mcp-agent-mail-share`
- **Issue:** `bundle_attachments` previously had hardcoded logic for absolute path resolution that didn't respect the `allow_absolute_attachment_paths` configuration setting.
- **Fix:**
    - Updated `resolve_attachment_path` helper in `crates/mcp-agent-mail-share/src/bundle.rs` to accept an `allow_absolute` flag.
    - Updated `bundle_attachments` public API to take `allow_absolute_attachment_paths` as an argument.
    - Updated `run_share_export` and `run_share_update` in `crates/mcp-agent-mail-cli/src/lib.rs` to propagate the configuration value from `Config`.
    - Updated unit tests in `crates/mcp-agent-mail-share/src/bundle.rs` to reflect the API change.

### 3. Guard Plugin (Pre-commit Hook) Logic Divergence
- **Component:** `crates/mcp-agent-mail-guard`
- **Issue:** The generated Python pre-commit hook used standard Python `fnmatch` (which allows wildcards to cross directory boundaries), whereas the Rust `guard_check` implementation used strict shell-style globs (where `*` does not match `/`). This caused inconsistency where the CLI tool (`am guard check`) might pass a change that the git hook would incorrectly block (or vice-versa). Additionally, the Python hook missed "reverse prefix" conflicts (e.g., replacing a directory containing reserved files with a file).
- **Fix:**
    - Modified `render_guard_plugin_script` in `crates/mcp-agent-mail-guard/src/lib.rs` to generate a Python script with a custom `glob_match` function (using regex) that strictly implements shell-style globbing (`*` vs `**`), matching the Rust behavior.
    - Added the missing reverse prefix check (`pattern.startswith(f + "/")`) to the Python script to correctly detect conflicts when reserved directories are modified.

## Verification
- Verified `mcp-agent-mail-server` and `mcp-agent-mail-tools` do not depend on `bundle_attachments` or are unaffected by the change.
- Confirmed `Config::from_env` correctly defaults `allow_absolute_attachment_paths` to `true` in development environments, preserving existing behavior while allowing restriction in production.
- Verified `mcp-agent-mail-guard`'s Rust implementation of `fnmatch_simple` aligns with the new Python `glob_match` logic.
