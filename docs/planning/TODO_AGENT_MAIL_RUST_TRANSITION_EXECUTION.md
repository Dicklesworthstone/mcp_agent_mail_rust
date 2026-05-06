# TODO: Agent Mail Rust Transition Execution

Date started: 2026-02-17  
Owner: Codex (this session)  
Scope: implement all requested deliverables from `AGENT_MAIL_RUST_VERSION_REPO_TRANSITION_PLAN.md`

## A. Work Tracking Rules

- [x] Maintain a granular checklist with explicit subtasks.
- [x] Mark each item complete only after code/docs are written and validated.
- [x] Keep this file updated as work proceeds.

## B. CLI Surface Changes

### B1. New top-level commands

- [x] Add `am legacy` command group to `Commands` enum.
- [x] Add `am upgrade` command to `Commands` enum.
- [x] Wire `Commands::Legacy` dispatch in `execute(...)`.
- [x] Wire `Commands::Upgrade` dispatch in `execute(...)`.

### B2. New subcommands under `am legacy`

- [x] Add `LegacyCommand::Detect`.
- [x] Add `LegacyCommand::Import`.
- [x] Add `LegacyCommand::Status`.
- [x] Add clap flags for output format (`--format` / `--json`) where appropriate.
- [x] Add clap flags for path overrides (`--search-root`, `--db`, `--storage-root`).
- [x] Add mode flags (`--auto`, `--in-place`, `--copy`, `--dry-run`, `--yes`).

### B3. Help/allowlist integrity

- [x] Update top-level help subcommand expectations in tests.
- [x] Ensure MCP denial contract tests remain valid (no command-routing changes were made to denial logic).

## C. Legacy Detection Implementation

### C1. Data model

- [x] Add internal structs for detection report, markers, candidate install(s), and confidence levels.
- [x] Define stable JSON shape for `am legacy detect --json`.

### C2. Marker collection

- [x] Detect Python repo markers (`pyproject.toml`, integration scripts, `uv.lock`, `.venv`).
- [x] Detect legacy env markers (`DATABASE_URL=sqlite+aiosqlite...`, `STORAGE_ROOT=~/.mcp_agent_mail_git_mailbox_repo`).
- [x] Detect DB markers (presence + schema signatures + legacy triggers).
- [x] Detect storage root existence and shape signals.

### C3. Source precedence resolution

- [x] Implement DB path precedence:
  - [x] explicit CLI arg
  - [x] env/process
  - [x] project `.env`
  - [x] user env file (`~/.mcp_agent_mail/.env`, `~/mcp_agent_mail/.env`)
  - [x] fallback defaults
- [x] Implement storage root precedence with same hierarchy.
- [x] Include resolved source provenance in output.

### C4. Detect command behavior

- [x] Implement `am legacy detect` human output.
- [x] Implement `am legacy detect --json` machine output.
- [x] Include actionable next-step guidance in non-JSON output.

## D. Legacy Import Implementation

### D1. Import planning

- [x] Build resolved import plan from explicit args or `--auto`.
- [x] Validate mandatory preconditions (paths exist/readable, db is sqlite file, mode coherence).
- [x] Emit dry-run operation plan with concrete paths.

### D2. Backups and safety

- [x] Create deterministic backup directory path and naming format.
- [x] Backup DB + `-wal` + `-shm` when present.
- [x] Backup storage root safely.
- [x] Ensure failures before migration leave source untouched.

### D3. Migration execution

- [x] Open source/target sqlite path using existing Rust DB helpers.
- [x] Run `schema::migrate_to_latest`.
- [x] Support `--in-place`.
- [x] Support `--copy` (copy source to target then migrate target).

### D4. Post-migration validation

- [x] Run `PRAGMA integrity_check`.
- [x] Verify core table accessibility and row-count sanity.
- [x] Verify expected migration table entries exist.
- [x] Emit validation summary.

### D5. Receipt and status artifact

- [x] Write receipt JSON with timestamp, mode, source/target, backups, migration result.
- [x] Store receipts in deterministic location under storage root.
- [x] Ensure receipt format can be consumed by `am legacy status`.

### D6. Setup refresh

- [x] Invoke setup refresh path (equivalent of `am setup run --yes`) after successful import.
- [x] Make refresh best-effort but clearly reported if partially failed.

### D7. Upgrade command behavior

- [x] Implement `am upgrade` orchestration:
  - [x] run detect
  - [x] if legacy found, run import flow (or print clear next command when interactive approval missing)
  - [x] run setup refresh
  - [x] emit final summary

## E. Legacy Status Implementation

- [x] Implement receipt discovery logic.
- [x] Implement `am legacy status` human output.
- [x] Implement `am legacy status --json`.
- [x] Show latest receipt + history count + outcome.

## F. Testing

### F1. Clap parsing tests

- [x] Add tests for `am legacy detect` flags and defaults.
- [x] Add tests for `am legacy import` flags/mode combinations.
- [x] Add tests for `am legacy status`.
- [x] Add tests for `am upgrade`.

### F2. Functional/unit tests

- [x] Add detection marker unit tests.
- [x] Add source-precedence tests (explicit-path precedence covered).
- [x] Add import dry-run plan tests.
- [x] Add backup path generation tests.
- [x] Add receipt serialization/readback tests.
- [x] Add status command behavior tests.

### F3. Legacy fixture integration tests

- [x] Use `legacy_python_mcp_agent_mail_code/mcp_agent_mail/storage.sqlite3` in an isolated temp copy.
- [x] Assert migrate/import succeeds and preserves key row counts.
- [x] Assert legacy trigger cleanup and timestamp conversion behavior.

### F4. Help text and command listing tests

- [x] Update tests that enumerate expected subcommands.
- [x] Update help tests to include new commands.

## G. Documentation Deliverables

### G1. In this Rust repo

- [x] Update main transition plan doc if implementation details changed.
- [x] Add old-repo cutover PR checklist template.
- [x] Add old-repo release notes template for Rust cutover.
- [x] Add operator-facing docs for `am legacy` and `am upgrade`.

### G2. Content quality

- [x] Include exact commands and rollback steps.
- [x] Include acceptance checklist for canary/GA transitions.

## H. Validation Gates

- [x] Run `cargo fmt --check` (targeted to changed Rust files).
- [x] Run targeted tests for modified CLI logic.
- [x] Run `cargo check --workspace --all-targets`.
- [x] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [x] Fix all failures/warnings.

## I. Validation Evidence

- [x] `cargo check -p mcp-agent-mail-cli --all-targets` -> PASS.
- [x] `cargo test -p mcp-agent-mail-cli legacy -- --nocapture` -> PASS.
- [x] `cargo test -p mcp-agent-mail-cli legacy::tests:: -- --nocapture` -> PASS.
- [x] `cargo test -p mcp-agent-mail-cli upgrade -- --nocapture` -> PASS.
- [x] `cargo check --workspace --all-targets` -> PASS.
- [x] `cargo clippy -p mcp-agent-mail-cli --all-targets --no-deps -- -D warnings` -> PASS.
- [x] `cargo clippy --workspace --all-targets -- -D warnings` -> PASS.
- [x] `cargo test -p mcp-agent-mail-storage --test stress_pipeline -- --list` -> PASS (stress test target compiles, test list emits 15 cases).

## K. Fresh-Eyes Hardening Pass (Post-Implementation)

- [x] Re-review all new legacy/upgrade code paths for correctness regressions.
- [x] Fix `am upgrade` non-interactive behavior to require `--yes`.
- [x] Add copy-mode overlap guard for source/target storage roots.
- [x] Add copy-mode guard rejecting pre-existing target DB paths.
- [x] Fix `am upgrade` to actually execute setup refresh after successful import.
- [x] Harden overlap normalization for paths containing `..` segments.
- [x] Fix `.env` parsing to handle `export KEY=VALUE` lines for legacy path auto-detection.
- [x] Expand `.env` parsing to handle `export<TAB>KEY=VALUE` format.
- [x] Fix env-marker detection so legacy `STORAGE_ROOT` alone still counts as a legacy signal.
- [x] Add preflight validation for source DB file type and source storage directory type.
- [x] Add preflight validation rejecting copy-mode target storage paths that are existing files.
- [x] Harden recursive storage copy against symlink-directory recursion and broken symlink ambiguity.
- [x] Add/extend regression tests covering all above edge cases.
- [x] Re-run `cargo fmt --check`.
- [x] Re-run focused legacy test suite.
- [x] Re-run workspace `cargo check`.
- [x] Re-run workspace `cargo clippy`.

## L. Old-Repo Cutover PR Body Artifact

- [x] Generate ready-to-paste cutover PR body from template.
- [x] Save artifact to `docs/OLD_REPO_RUST_CUTOVER_PR_BODY.md`.

## J. Finalization

- [x] Update this TODO file with completed states.
- [x] Produce final summary with changed files and behavior notes.
- [x] Include any residual risks or follow-up tasks.
