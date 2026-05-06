# AGENT_MAIL_RUST_VERSION_REPO_TRANSITION_PLAN

Status: Active (initial Rust-side implementation completed in this repo on 2026-02-17)  
Date: 2026-02-17  
Primary objective: convert `mcp_agent_mail` (Python, high-star canonical repo) into the Rust implementation while preserving user trust, install paths, and existing mailbox data.

## 0. Execution Snapshot (Rust Repo)

The following transition-critical pieces are now implemented in this Rust repo:

1. `am legacy detect` with marker scoring + source-precedence reporting.
2. `am legacy import` with `--auto`, explicit path overrides, `--in-place`, `--copy`, `--dry-run`, and receipt writing.
3. `am legacy status` receipt discovery/reporting.
4. `am upgrade` orchestration flow (detect -> import-or-setup-refresh -> summary).
5. Setup refresh invocation wired into import/upgrade flows.

Remaining cutover work happens in the old canonical repo (`mcp_agent_mail`) using the templates and rollout phases in this document.

## 1. Problem Statement

The Python repo (`/dp/mcp_agent_mail`) has the stars and ecosystem gravity, while the Rust repo (`/dp/mcp_agent_mail_rust`) is the technical replacement.  
Without a controlled transition, users will fragment across two repos, legacy install scripts will drift, and existing SQLite/archive data may be lost or orphaned.

## 2. Success Criteria

1. The canonical public repo with existing stars becomes Rust-first on `main`.
2. Existing install paths from the Python repo still work and install the Rust binaries.
3. Existing Python-era data (`storage.sqlite3` + Git storage root) is auto-detected and imported safely.
4. Existing users can upgrade with one command and no manual SQL work.
5. Rollback is deterministic and documented.
6. Rust repo and old repo do not diverge after cutover.

## 3. Non-Goals

1. Long-term support for running the Python server as a first-class mode.
2. Perfect byte-for-byte replay of every historical Python-only runtime quirk.
3. Multi-month compatibility shims with unclear sunset dates.

## 4. Recommended High-Level Strategy

Use a staged canonical-repo cutover:

1. Make Python repo the canonical Rust distribution target.
2. Preserve Python code on a dedicated branch/tag (`legacy-python`) for archaeology and emergency rollback.
3. Keep old install URLs alive by turning them into Rust installers.
4. Add first-class `am` legacy-detection/import workflow before cutover.
5. Roll out with explicit canary rings and hard go/no-go gates.

## 5. Repository Transition Design

### 5.1 Target Repo Topology

1. Canonical repo after cutover: `mcp_agent_mail` (stars retained).
2. Default branch: `main` (Rust codebase).
3. Legacy Python preservation:
   - branch: `legacy-python`
   - immutable tag: `python-final-v1.x`
4. Rust repo post-cutover:
   - either mirror-only, or archived with README redirect to canonical repo.

### 5.2 Git Transition Procedure (No History Loss)

1. Freeze Python repo for cutover window.
2. In Python repo:
   - fetch Rust repo remote
   - merge/import Rust `main` into a cutover branch
   - preserve Python snapshot in `legacy-python`
3. Validate build/test/install in cutover branch.
4. Promote cutover branch to `main`.
5. Push `main` to `master` for compatibility (`git push origin main:master`).

### 5.3 Release Semantics

1. Last Python release: `v1.x` (explicitly marked legacy/frozen).
2. First canonical Rust release in old repo: `v2.0.0`.
3. Release notes must include:
   - one-command migration path
   - rollback command path
   - known differences and removed behaviors

## 6. Installer and Entry-Point Compatibility Plan

### 6.1 Keep Existing URLs Working

1. Replace legacy `scripts/install.sh` in old repo with a Rust installer entrypoint.
2. Preserve existing curl command compatibility:
   - `https://raw.githubusercontent.com/.../mcp_agent_mail/main/scripts/install.sh`
3. Script behavior:
   - install `am` + `mcp-agent-mail`
   - optionally run legacy conversion (`am legacy import --auto --yes`)
   - run `am setup run --yes`

### 6.2 Command Compatibility Policy

1. Native path is authoritative: `am ...` and `mcp-agent-mail ...`.
2. Legacy wrappers (if kept) are time-bounded and emit migration guidance.
3. Any wrapper must be a forwarding shim only, never separate logic.

## 7. New `am` Legacy Detection and Import Feature

This is the key requirement for “everything just works.”

### 7.1 Proposed CLI Surface

1. `am legacy detect [--json] [--search-root <path>]`
2. `am legacy import --auto [--yes] [--dry-run]`
3. `am legacy import --db <path> --storage-root <path> [--in-place|--copy] [--yes] [--dry-run]`
4. `am legacy status [--json]` (shows previous migration receipts)

Optional convenience:
1. `am upgrade` triggers `legacy detect` and offers/executes import when applicable.

### 7.2 Detection Heuristics

A legacy install is “detected” if at least one high-confidence marker is present.

High-confidence markers:
1. `pyproject.toml` with package `mcp-agent-mail`.
2. Legacy scripts present (`scripts/run_server_with_token.sh`, integration scripts).
3. Legacy `.env` with Python defaults:
   - `DATABASE_URL=sqlite+aiosqlite:///...`
   - `STORAGE_ROOT=~/.mcp_agent_mail_git_mailbox_repo`
4. Legacy DB found with Python schema signatures:
   - DATETIME/TEXT timestamp columns
   - triggers named `fts_messages_ai/ad/au`

Medium-confidence markers:
1. `.venv` and `uv.lock` adjacent to `mcp_agent_mail`.
2. Existing `storage.sqlite3` plus storage repo path.

### 7.3 Source Resolution Precedence

1. Explicit CLI flags (`--db`, `--storage-root`)
2. Process env (`DATABASE_URL`, `STORAGE_ROOT`)
3. Project `.env`
4. User env file:
   - `~/.mcp_agent_mail/.env`
   - `~/mcp_agent_mail/.env`
5. Legacy defaults:
   - DB: `./storage.sqlite3` (or parsed from python URL)
   - storage: `~/.mcp_agent_mail_git_mailbox_repo`

### 7.4 Import Modes

1. `in-place` (preferred default):
   - run migrations directly against detected legacy DB
   - keep existing storage root
2. `copy`:
   - snapshot-copy DB + storage root into target paths
   - migrate copied DB
   - keep source untouched

### 7.5 Import Algorithm

1. Preflight:
   - resolve DB/storage paths
   - assert path existence/readability
   - print plan and mode
2. Safety backup:
   - create timestamped DB backup (`.sqlite3`, `-wal`, `-shm`)
   - archive storage root (or copy to backup dir)
3. Migration:
   - open DB via Rust pool/parser (supports `sqlite+aiosqlite`)
   - run `schema::migrate_to_latest`
4. Post-migration normalization:
   - WAL checkpoint
   - ensure FTS/triggers consistent
5. Validation:
   - `PRAGMA integrity_check`
   - row-count checks on core tables
   - sample inbox/search query sanity
6. Receipt:
   - write JSON receipt with source, target, backups, migration IDs, timestamp
7. Config rewrite:
   - run `am setup run --yes` to refresh agent MCP configs

### 7.6 Why This Works with Existing Data

The Rust schema migrations already handle Python-era data shape:

1. Drops legacy Python FTS triggers that conflict with Rust triggers.
2. Converts legacy TEXT/DATETIME timestamps to integer microseconds.
3. Adds missing modern tables/indexes incrementally and idempotently.
4. Accepts Python-style SQLite URLs (`sqlite+aiosqlite:///...`).

### 7.7 Failure and Rollback Rules

If any import stage fails:

1. stop immediately
2. do not continue with partial post-steps
3. print rollback instructions with exact backup paths
4. return non-zero exit code

Rollback command path should be one command in docs:
1. restore DB backup
2. restore storage-root backup
3. rerun `am doctor check --json`

## 8. Rust Codebase Implementation Plan

### 8.1 CLI and Core Changes

1. `crates/mcp-agent-mail-cli/src/lib.rs`
   - add `LegacyCommand` enum and handlers
   - wire `Commands::Legacy`
2. `crates/mcp-agent-mail-core/src/config.rs`
   - reuse current env precedence helpers for detection source tracing
3. `crates/mcp-agent-mail-db/src/schema.rs`
   - no new migration logic expected for baseline import
   - add targeted migration tests only if gaps found
4. `crates/mcp-agent-mail-core/src/setup.rs`
   - add optional rewrite support for legacy command-based MCP entries if discovered

### 8.2 Test Additions

1. Unit tests:
   - detection scoring and source resolution
   - db URL normalization (`sqlite+aiosqlite` and `sqlite:///`)
2. Integration tests:
   - run import against copied legacy fixture DB
   - assert migrated schema and row preservation
3. E2E:
   - add `tests/e2e/test_legacy_import.sh`
   - dry-run, success path, corrupted-db failure path, rollback path

### 8.3 Validation Gates

1. `cargo check --workspace --all-targets`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo fmt --check`
4. `cargo test --workspace`
5. new E2E legacy-import script must pass

## 9. Canonical Repo Cutover Execution Plan

### Phase 0: Pre-Cutover Readiness

1. Implement and ship `am legacy detect/import` in Rust repo.
2. Validate migration with real legacy DB fixtures.
3. Dry-run cutover in a fork of Python repo.

Exit criteria:
1. migration feature green
2. installer compatibility script verified
3. rollback tested end-to-end

### Phase 1: Canary Cutover in Old Repo

1. Create cutover branch in `mcp_agent_mail`.
2. Import Rust code and preserve Python branch/tag.
3. Replace old installer entrypoint with Rust bootstrap behavior.
4. Publish RC release notes (`v2.0.0-rc1`).

Exit criteria:
1. fresh install works from old URL
2. legacy import works on at least 3 real legacy environments
3. no critical regressions in CI/E2E

### Phase 2: GA Cutover

1. Merge cutover branch to `main` in old repo.
2. Tag `v2.0.0`.
3. Sync `main` to `master`.
4. Update README, pinned issue, and migration docs in old repo.

### Phase 3: Post-Cutover Stabilization (2-4 weeks)

1. Monitor issue volume and import failure rates.
2. Ship fast fixes for migration edge cases.
3. Keep Python branch read-only except critical security patches.

## 10. Documentation and Communication Plan

Required artifacts in canonical repo:

1. `MIGRATION_TO_RUST.md` (user-facing one-command migration path)
2. `LEGACY_PYTHON_SUPPORT_POLICY.md` (scope + sunset)
3. `ROLLBACK_FROM_RUST.md` (backup/restore playbook)
4. release notes with copy-paste commands

Communication sequence:

1. pre-announcement issue in old repo
2. RC announcement with canary instructions
3. GA announcement with hard dates and shim policy
4. post-GA summary with known issues and fixes

## 11. Risk Register

| Risk | Impact | Mitigation | Rollback Trigger |
|---|---|---|---|
| Legacy DB edge schema not covered | High | Fixture corpus + canary conversions | Import failure rate > 2% |
| Installer breakage from old URL | High | Preserve script path and test with curl flows | Install success < 99% |
| Config rewrite misses some agent clients | Medium | Keep `am setup run` plus targeted rewrite patterns | Multiple reports for same client |
| Partial migration leaves mixed state | High | transactional sequencing + required backup before apply | Any failed post-migrate validation |
| Repo confusion (two public repos) | Medium | clear canonical designation + redirects | User support load spikes |

## 12. Milestone Backlog (Execution Order)

1. Add `am legacy detect` (read-only).
2. Add `am legacy import --dry-run`.
3. Add `am legacy import --apply` with backups + receipts.
4. Add E2E legacy import suite.
5. Update Rust install script for optional auto-import.
6. Dry-run full cutover in old repo fork.
7. Execute canary cutover.
8. Execute GA cutover.
9. Stabilization window and shim cleanup.

## 13. Definition of Done

1. Running the old installer URL yields working Rust binaries.
2. A legacy Python user can run one command (`am legacy import --auto --yes`) and keep all mailbox data.
3. Core commands (`am`, `mcp-agent-mail`, setup, inbox/search, reservations) work immediately after import.
4. Old repo is canonical and Rust-first on `main`.
5. Legacy Python code remains retrievable via branch/tag without polluting new default workflow.

## 14. Immediate Next Actions

1. Implement `am legacy detect` and `am legacy import` in this Rust repo first.
2. Add fixture-backed tests using `legacy_python_mcp_agent_mail_code/mcp_agent_mail/storage.sqlite3`.
3. Draft the old-repo cutover PR template and release notes before touching canonical `main`.
