# Baseline - 2026-04-25

The baseline gate was not green before the simplification loop produced any new
code changes.

## Commands

- `cargo fmt --check` - passed before the long baseline.
- `TEST_CMD='rch exec -- cargo test --workspace --no-fail-fast' LINT_CMD='rch exec -- cargo clippy --workspace --all-targets -- -D warnings' /home/ubuntu/.codex/skills/simplify-and-refactor-code-isomorphically/scripts/baseline.sh 2026-04-25-silentoriole-simplify` - interrupted after prolonged silence while DB tests were running.

## Observed Pre-Existing Failures

- `mcp-agent-mail-cli` lib test run reported 31 failures, including lock contention on the default storage root, snapshot drift, temp-project guard failures, FTS prefix failure, archive/share view failures, and dual-mode/help matrix drift.
- `mcp-agent-mail-core` lib reported two flag-default failures:
  `flags::tests::registry_defaults_match_config_defaults` and
  `flags::tests::toggle_dynamic_bool_flag_writes_console_envfile`.
- The run then proceeded into `mcp-agent-mail-db` tests and stayed silent long
  enough that the orchestrator interrupted it to avoid consuming the build lane
  indefinitely.

## Policy For This Loop

Because the workspace baseline is already red, each pass uses focused proof for
the touched surface: source diff inspection, `cargo fmt --check`, `git diff
--check`, `ubs` when available, and targeted crate tests when practical. Full
workspace-green claims are not made until the pre-existing baseline failures are
resolved separately.
