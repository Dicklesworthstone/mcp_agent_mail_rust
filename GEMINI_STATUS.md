# Gemini Status - 2026-02-20

## Status
**State:** Maintenance & Hardening
**Active Agent:** Gemini

## Completed Tasks
- [x] Fix `ensure_project` Windows absolute path handling in `mcp-agent-mail-db`.
- [x] Refactor `bundle_attachments` in `mcp-agent-mail-share` to respect `allow_absolute_attachment_paths` config.
- [x] Update `mcp-agent-mail-cli` to pass configuration to `bundle_attachments`.
- [x] Verify no regressions in `mcp-agent-mail-server` or `mcp-agent-mail-tools`.
- [x] Fix logic divergence in `mcp-agent-mail-guard` generated Python hooks (glob matching & prefix checks).
- [x] Fix git author misattribution in `mcp-agent-mail-storage` commit coalescer (batching logic).
- [x] Unify `reply_message` logic with `send_message` (contact enforcement + auto-registration) in `mcp-agent-mail-tools`.
- [x] Fix `send_message` broadcast behavior to skip blocked recipients instead of failing (crates/mcp-agent-mail-tools).
- [x] Fix `release_file_reservations` race condition and ensure archive artifacts are updated on release (crates/mcp-agent-mail-db, crates/mcp-agent-mail-tools).
- [x] Fix `budget_deadline_is_relative_to_wall_now_not_absolute` test logic error in `mcp-agent-mail-server` to correctly use `saturating_duration_since`.
- [x] Enable pre-push hook installation in `mcp-agent-mail-guard` and `mcp-agent-mail-tools` (restoring legacy parity).
- [x] Fix `budget_deadline_is_relative_to_wall_now_not_absolute` test logic error in `mcp-agent-mail-server` to correctly use `saturating_duration_since`.
- [x] Fix `handle_guard` in `mcp-agent-mail-cli` to correctly pass `prepush` flags to `install_guard`.

## Pending
- None identified.

## Notes
- `run_shell_command` is unreliable (Signal 1/SIGHUP), so verification was performed via static analysis and code reading.
- `release_reservations` DB query now returns `Vec<FileReservationRow>` (using `RETURNING`) to enable precise artifact updates.
- `send_message` broadcast now gracefully handles mixed contact policies by dropping restricted recipients.
- Pre-push hooks are now installed by default via `install_precommit_guard` tool and `am guard install`.
