# AGENTS.md — MCP Agent Mail (Rust)

> Guidelines for AI coding agents working in this Rust codebase.

---

## RULE 0 - THE FUNDAMENTAL OVERRIDE PREROGATIVE

If I tell you to do something, even if it goes against what follows below, YOU MUST LISTEN TO ME. I AM IN CHARGE, NOT YOU.

---

## RULE NUMBER 1: NO FILE DELETION

**YOU ARE NEVER ALLOWED TO DELETE A FILE WITHOUT EXPRESS PERMISSION.** Even a new file that you yourself created, such as a test code file. You have a horrible track record of deleting critically important files or otherwise throwing away tons of expensive work. As a result, you have permanently lost any and all rights to determine that a file or folder should be deleted.

**YOU MUST ALWAYS ASK AND RECEIVE CLEAR, WRITTEN PERMISSION BEFORE EVER DELETING A FILE OR FOLDER OF ANY KIND.**

---

## Irreversible Git & Filesystem Actions — DO NOT EVER BREAK GLASS

> **Note:** Multi-agent coordination depends on clean, predictable state. Destructive commands can destroy the work of many agents simultaneously.

1. **Absolutely forbidden commands:** `git reset --hard`, `git clean -fd`, `rm -rf`, or any command that can delete or overwrite code/data must never be run unless the user explicitly provides the exact command and states, in the same message, that they understand and want the irreversible consequences.
2. **No guessing:** If there is any uncertainty about what a command might delete or overwrite, stop immediately and ask the user for specific approval. "I think it's safe" is never acceptable.
3. **Safer alternatives first:** When cleanup or rollbacks are needed, request permission to use non-destructive options (`git status`, `git diff`, `git stash`, copying to backups) before ever considering a destructive command.
4. **Mandatory explicit plan:** Even after explicit user authorization, restate the command verbatim, list exactly what will be affected, and wait for a confirmation that your understanding is correct. Only then may you execute it—if anything remains ambiguous, refuse and escalate.
5. **Document the confirmation:** When running any approved destructive command, record (in the session notes / final response) the exact user text that authorized it, the command actually run, and the execution time. If that record is absent, the operation did not happen.

---

## Git Branch: ONLY Use `main`, NEVER `master`

**The default branch is `main`. The `master` branch exists only for legacy URL compatibility.**

- **All work happens on `main`** — commits, PRs, feature branches all merge to `main`
- **Never reference `master` in code or docs** — if you see `master` anywhere, it's a bug that needs fixing
- **The `master` branch must stay synchronized with `main`** — after pushing to `main`, also push to `master`:
  ```bash
  git push origin main:master
  ```

**Why this matters:** Some references and install URLs historically referenced `master`. If `master` falls behind `main`, users get stale code.

**If you see `master` referenced anywhere:**
1. Update it to `main`
2. Ensure `master` is synchronized: `git push origin main:master`

---

## Toolchain: Rust & Cargo

We only use **Cargo** in this project, NEVER any other package manager.

- **Edition:** Rust 2024 (nightly required — see `rust-toolchain.toml`)
- **Dependency versions:** Explicit versions for stability
- **Configuration:** Cargo.toml only
- **Unsafe code:** Forbidden (`#![forbid(unsafe_code)]`)

### Key Dependencies

| Crate | Purpose |
|-------|---------|
| `fastmcp_rust` (`/dp/fastmcp_rust`) | MCP protocol implementation (MUST use, not tokio) |
| `sqlmodel_rust` (`/dp/sqlmodel_rust`) | SQLite ORM (MUST use, not rusqlite) |
| `asupersync` (`/dp/asupersync`) | Async runtime (MUST use, not tokio) |
| `frankentui` (`/dp/frankentui`) | TUI rendering for operations console |
| `beads_rust` (`/dp/beads_rust`) | Issue tracking integration |
| `coding_agent_session_search` (`/dp/coding_agent_session_search`) | Agent detection |
| `serde` + `serde_json` | JSON serialization for MCP protocol |
| `chrono` | Timestamp handling (i64 microseconds since epoch) |

---

## Code Editing Discipline

### No Script-Based Changes

**NEVER** run a script that processes/changes code files in this repo. Brittle regex-based transformations create far more problems than they solve.

- **Always make code changes manually**, even when there are many instances
- For many simple changes: use parallel subagents
- For subtle/complex changes: do them methodically yourself

### No File Proliferation

If you want to change something or add a feature, **revise existing code files in place**.

**NEVER** create variations like:
- `mainV2.rs`
- `main_improved.rs`
- `main_enhanced.rs`

New files are reserved for **genuinely new functionality** that makes zero sense to include in any existing file. The bar for creating new files is **incredibly high**.

---

## Backwards Compatibility

We do not care about backwards compatibility—we're in early development with no users. We want to do things the **RIGHT** way with **NO TECH DEBT**.

- Never create "compatibility shims"
- Never create wrapper functions for deprecated APIs
- Just fix the code directly

---

## Output Style

This project exposes two interfaces with different output conventions:

- **MCP server (`mcp-agent-mail`):** JSON-RPC responses over stdio or HTTP. Tool results are JSON objects.
- **CLI (`am`):** TTY-aware tables for humans, `--json` flag for machine-readable output.

Dual-mode behavior:
- **MCP mode (default):** CLI-only commands produce a deterministic denial on stderr with exit code `2`
- **CLI mode (`AM_INTERFACE_MODE=cli`):** MCP-only commands denied with guidance pointing back to MCP mode
- **`scripts/am`:** Dev convenience wrapper around `mcp-agent-mail serve` (HTTP + TUI)

---

## Compiler Checks (CRITICAL)

**After any substantive code changes, you MUST verify no errors were introduced:**

```bash
# Check for compiler errors and warnings
cargo check --all-targets

# Check for clippy lints (pedantic + nursery are enabled)
cargo clippy --all-targets -- -D warnings

# Verify formatting
cargo fmt --check
```

If you see errors, **carefully understand and resolve each issue**. Read sufficient context to fix them the RIGHT way.

---

## Testing

### Unit Tests

The workspace includes 1000+ tests across all crates:

```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture

# Run specific crate
cargo test -p mcp-agent-mail-db
cargo test -p mcp-agent-mail-tools
cargo test -p mcp-agent-mail-server

# Conformance tests (parity with Python reference)
cargo test -p mcp-agent-mail-conformance

# Benchmarks
cargo bench -p mcp-agent-mail
```

### End-to-End Testing

```bash
# E2E test suites (37 scripts in tests/e2e/)
tests/e2e/test_stdio.sh       # MCP stdio transport (17 assertions)
tests/e2e/test_http.sh        # HTTP transport (47 assertions)
tests/e2e/test_guard.sh       # Pre-commit guard (32 assertions)
tests/e2e/test_macros.sh      # Macro tools (20 assertions)
tests/e2e/test_share.sh       # Share/export (44 assertions)
tests/e2e/test_dual_mode.sh   # Mode switching (84+ assertions)
tests/e2e/test_jwt.sh         # JWT authentication
scripts/e2e_cli.sh            # CLI integration (99 assertions)
```

### Test Categories

| Area | Tests | Purpose |
|------|-------|---------|
| DB queries + pool | 38+ | Storage layer correctness |
| Stress tests | 9 | Concurrent ops, pool exhaustion, cache coherency |
| Tool implementations | 34 | All MCP tools via conformance fixtures |
| Resources | 33+ | All MCP resources vs Python parity |
| CLI commands | 123+ | All 40+ CLI commands |
| Guard | 34 unit + 8 E2E | Pre-commit reservation enforcement |
| Share/export | 62 | Snapshot, scrub, bundle, crypto pipeline |
| FTS sanitization | 20 | Full-text search edge cases |
| LLM integration | 20 | Model selection, completion, merge logic |
| Dual-mode | 42+ | Mode matrix (CLI-allow + MCP-deny) |

---

## Quality Gates

Before committing, run these checks:

```bash
# Format, lint, test — the mandatory trifecta
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

# Conformance against Python reference (optional but recommended)
cargo test -p mcp-agent-mail-conformance

# Multi-agent builds: isolate target dir to avoid lock contention
export CARGO_TARGET_DIR="/tmp/target-$(whoami)-am"
```

### Debugging Test Failures

#### Conformance Failure
1. Run `cargo test -p mcp-agent-mail-conformance -- --nocapture`
2. Compare Rust output against Python fixtures in `tests/conformance/fixtures/`
3. Regenerate fixtures with `python_reference/generate_fixtures.py` if Python behavior changed

#### E2E Failure
1. Run the specific E2E script with bash `-x` for tracing
2. Check that `mcp-agent-mail` binary builds: `cargo build -p mcp-agent-mail`
3. Verify server starts on expected port: `scripts/am --no-tui`

#### Stress Test Failure
1. Run with `--nocapture --test-threads=1` for isolation
2. Check for SQLite lock contention (increase pool size or use WAL checkpoint)
3. Review circuit breaker state in test output

---

## Commit Process

When changes are ready, follow this process:

### 1. Verify Quality Gates Locally

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

### 2. Commit Changes

```bash
git add <specific-files>
git commit -m "fix: description of fixes

- List specific fixes
- Include any breaking changes

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

### 3. Push

```bash
git push origin main
```

---

## Third-Party Library Usage

If you aren't 100% sure how to use a third-party library, **SEARCH ONLINE** to find the latest documentation and current best practices.

---

## MCP Agent Mail — This Project

**This is the project you're working on.** MCP Agent Mail is a mail-like coordination layer for coding agents, providing an MCP server with 34 tools and 20+ resources, Git-backed archive, SQLite indexing, and an interactive TUI operations console.

### Architecture

Cargo workspace with strict dependency layering:

```text
mcp-agent-mail-core          (config, models, errors, metrics)
  ├─ mcp-agent-mail-db       (SQLite schema, queries, pool)
  ├─ mcp-agent-mail-storage  (Git archive, commit coalescer)
  ├─ mcp-agent-mail-guard    (pre-commit guard, reservation enforcement)
  ├─ mcp-agent-mail-share    (snapshot, bundle, export)
  └─ mcp-agent-mail-tools    (34 MCP tool implementations)
       └─ mcp-agent-mail-server  (HTTP/MCP runtime, TUI, web UI)
            ├─ mcp-agent-mail        (server binary)
            ├─ mcp-agent-mail-cli    (am CLI binary)
            └─ mcp-agent-mail-conformance  (parity tests)
```

### Key Files

| Path | Purpose |
|------|---------|
| `Cargo.toml` | Workspace manifest with all 10 crates |
| `crates/mcp-agent-mail/src/main.rs` | Server binary entry point (dual-mode) |
| `crates/mcp-agent-mail-cli/src/main.rs` | CLI binary (`am`) entry point |
| `crates/mcp-agent-mail-core/src/config.rs` | 100+ environment variables |
| `crates/mcp-agent-mail-core/src/models.rs` | Core data models (Project, Agent, Message, etc.) |
| `crates/mcp-agent-mail-db/src/queries.rs` | All SQL queries (instrumented) |
| `crates/mcp-agent-mail-tools/src/` | 34 MCP tool implementations |
| `crates/mcp-agent-mail-server/src/lib.rs` | Server dispatch, HTTP handler |
| `crates/mcp-agent-mail-server/src/tui_*.rs` | TUI operations console (11 screens) |
| `scripts/am` | Dev convenience wrapper (HTTP + TUI) |
| `tests/e2e/` | 37 E2E test scripts |
| `rust-toolchain.toml` | Nightly toolchain requirement |

### 34 MCP Tools (9 Clusters)

| Cluster | Count | Tools |
|---------|-------|-------|
| Infrastructure | 4 | health_check, ensure_project, ensure_product, products_link |
| Identity | 3 | register_agent, create_agent_identity, whois |
| Messaging | 5 | send_message, reply_message, fetch_inbox, acknowledge_message, mark_message_read |
| Contacts | 4 | request_contact, respond_contact, list_contacts, set_contact_policy |
| File Reservations | 4 | file_reservation_paths, renew_file_reservations, release_file_reservations, force_release_file_reservation |
| Search | 2 | search_messages, summarize_thread |
| Macros | 4 | macro_start_session, macro_prepare_thread, macro_contact_handshake, macro_file_reservation_cycle |
| Product Bus | 5 | ensure_product, products_link, search_messages_product, fetch_inbox_product, summarize_thread_product |
| Build Slots | 3 | acquire_build_slot, renew_build_slot, release_build_slot |

### 11-Screen TUI

| # | Screen | Shows |
|---|--------|-------|
| 1 | Dashboard | Real-time event stream with sparkline |
| 2 | Messages | Message browser with search and filtering |
| 3 | Threads | Thread view with correlation |
| 4 | Agents | Registered agents with activity indicators |
| 5 | Search | Query bar + facets + results + preview |
| 6 | Reservations | File reservations with TTL countdowns |
| 7 | Tool Metrics | Per-tool latency and call counts |
| 8 | SystemHealth | Connection probes, disk/memory, circuit breakers |
| 9 | Timeline | Chronological event timeline with inspector |
| 10 | Projects | Project list and routing helpers |
| 11 | Contacts | Contact graph and policy surface |

Key bindings: `?` help, `Ctrl+P` command palette, `m` toggle MCP/API, `Shift+T` cycle theme, `q` quit.

### File Reservations for Multi-Agent Editing

Before editing, agents should reserve file paths to avoid conflicts:

| Area | Reserve glob |
|------|-------------|
| Core types/config | `crates/mcp-agent-mail-core/src/**` |
| SQLite layer | `crates/mcp-agent-mail-db/src/**` |
| Git archive | `crates/mcp-agent-mail-storage/src/**` |
| Tool implementations | `crates/mcp-agent-mail-tools/src/**` |
| TUI | `crates/mcp-agent-mail-server/src/tui_*.rs` |
| CLI/launcher | `crates/mcp-agent-mail-cli/src/**`, `scripts/am` |

### Exit Codes

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | Runtime error (DB unreachable, tool failure, etc.) |
| `2` | Usage error (wrong interface mode, invalid flags) |

### Configuration

All configuration via environment variables. Key variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `AM_INTERFACE_MODE` | (unset = MCP) | `mcp` or `cli` (ADR-002) |
| `HTTP_HOST` | `127.0.0.1` | Bind address |
| `HTTP_PORT` | `8765` | Bind port |
| `HTTP_PATH` | `/mcp/` | MCP base path |
| `HTTP_BEARER_TOKEN` | (from `.env` file) | Auth token |
| `DATABASE_URL` | `sqlite:///:memory:` | SQLite connection URL |
| `STORAGE_ROOT` | `~/.mcp_agent_mail` | Archive root directory |
| `TUI_ENABLED` | `true` | Interactive TUI toggle |
| `WORKTREES_ENABLED` | `false` | Build slots feature flag |

For the full list of 100+ env vars, see `crates/mcp-agent-mail-core/src/config.rs`.

### Quick Start

```bash
scripts/am                  # HTTP server with TUI on 127.0.0.1:8765
scripts/am --api            # Use /api/ transport instead of /mcp/
scripts/am --no-tui         # Headless server (no interactive TUI)
scripts/am --no-auth        # Skip authentication (local dev)
cargo run -p mcp-agent-mail # stdio transport (for MCP client integration)
cargo run -p mcp-agent-mail-cli -- --help  # CLI tool
```

---

## MCP Agent Mail — Multi-Agent Coordination

A mail-like layer that lets coding agents coordinate asynchronously via MCP tools and resources. Provides identities, inbox/outbox, searchable threads, and advisory file reservations with human-auditable artifacts in Git.

### Why It's Useful

- **Prevents conflicts:** Explicit file reservations (leases) for files/globs
- **Token-efficient:** Messages stored in per-project archive, not in context
- **Quick reads:** `resource://inbox/...`, `resource://thread/...`

### Dual-Mode Reminder (MCP Server vs CLI)

This project intentionally keeps **MCP server** and **CLI** command surfaces separate:

- MCP server binary: `mcp-agent-mail` (default: MCP stdio; `serve` for HTTP; `config` for debugging)
- CLI binary: `am` (built by the `mcp-agent-mail-cli` crate)

If you accidentally run a CLI-only command via the MCP binary (e.g. `mcp-agent-mail share ...`),
it should deny with a deterministic message and exit code `2` telling you to use the CLI.

Note: `scripts/am` is a dev wrapper around `mcp-agent-mail serve` (HTTP + TUI). It is not the `am` CLI binary.

### Port Discipline During Migration (Critical)

`127.0.0.1:8765` is the canonical Agent Mail endpoint used by local coding agents. Keep behavior stable on this port.

- Do not run port-kill commands targeting `8765` (for example `fuser -k 8765/tcp`) unless the user explicitly requests it in this session.
- If an Agent Mail server is already running on `8765`, reuse it instead of replacing it.
- Any Rust-side change that affects externally observed behavior on `8765` must maintain Python parity or be treated as a bug.

### Same Repository Workflow

1. **Register identity:**
   ```
   ensure_project(project_key=<abs-path>)
   register_agent(project_key, program, model)
   ```

2. **Reserve files before editing:**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true)
   ```

3. **Communicate with threads:**
   ```
   send_message(..., thread_id="FEAT-123")
   fetch_inbox(project_key, agent_name)
   acknowledge_message(project_key, agent_name, message_id)
   ```

4. **Quick reads:**
   ```
   resource://inbox/{Agent}?project=<abs-path>&limit=20
   resource://thread/{id}?project=<abs-path>&include_bodies=true
   ```

### Macros vs Granular Tools

- **Prefer macros for speed:** `macro_start_session`, `macro_prepare_thread`, `macro_file_reservation_cycle`, `macro_contact_handshake`
- **Use granular tools for control:** `register_agent`, `file_reservation_paths`, `send_message`, `fetch_inbox`, `acknowledge_message`

### Common Pitfalls

- `"from_agent not registered"`: Always `register_agent` in the correct `project_key` first
- `"FILE_RESERVATION_CONFLICT"`: Adjust patterns, wait for expiry, or use non-exclusive reservation
- **Auth errors:** If JWT+JWKS enabled, include bearer token with matching `kid`

---

## Beads (br) — Dependency-Aware Issue Tracking

Beads provides a lightweight, dependency-aware issue database and CLI (`br` - beads_rust) for selecting "ready work," setting priorities, and tracking status. It complements MCP Agent Mail's messaging and file reservations.

**Important:** `br` is non-invasive—it NEVER runs git commands automatically. You must manually commit changes after `br sync --flush-only`.

### Conventions

- **Single source of truth:** Beads for task status/priority/dependencies; Agent Mail for conversation and audit
- **Shared identifiers:** Use Beads issue ID (e.g., `br-123`) as Mail `thread_id` and prefix subjects with `[br-123]`
- **Reservations:** When starting a task, call `file_reservation_paths()` with the issue ID in `reason`

### Typical Agent Flow

1. **Pick ready work (Beads):**
   ```bash
   br ready --json  # Choose highest priority, no blockers
   ```

2. **Reserve edit surface (Mail):**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true, reason="br-123")
   ```

3. **Announce start (Mail):**
   ```
   send_message(..., thread_id="br-123", subject="[br-123] Start: <title>", ack_required=true)
   ```

4. **Work and update:** Reply in-thread with progress

5. **Complete and release:**
   ```bash
   br close 123 --reason "Completed"
   br sync --flush-only  # Export to JSONL (no git operations)
   ```
   ```
   release_file_reservations(project_key, agent_name, paths=["src/**"])
   ```
   Final Mail reply: `[br-123] Completed` with summary

### Mapping Cheat Sheet

| Concept | Value |
|---------|-------|
| Mail `thread_id` | `br-###` |
| Mail subject | `[br-###] ...` |
| File reservation `reason` | `br-###` |
| Commit messages | Include `br-###` for traceability |

---

## bv — Graph-Aware Triage Engine

bv is a graph-aware triage engine for Beads projects (`.beads/beads.jsonl`). It computes PageRank, betweenness, critical path, cycles, HITS, eigenvector, and k-core metrics deterministically.

**Scope boundary:** bv handles *what to work on* (triage, priority, planning). For agent-to-agent coordination (messaging, work claiming, file reservations), use MCP Agent Mail.

**CRITICAL: Use ONLY `--robot-*` flags. Bare `bv` launches an interactive TUI that blocks your session.**

### The Workflow: Start With Triage

**`bv --robot-triage` is your single entry point.** It returns:
- `quick_ref`: at-a-glance counts + top 3 picks
- `recommendations`: ranked actionable items with scores, reasons, unblock info
- `quick_wins`: low-effort high-impact items
- `blockers_to_clear`: items that unblock the most downstream work
- `project_health`: status/type/priority distributions, graph metrics
- `commands`: copy-paste shell commands for next steps

```bash
bv --robot-triage        # THE MEGA-COMMAND: start here
bv --robot-next          # Minimal: just the single top pick + claim command
```

### Command Reference

**Planning:**
| Command | Returns |
|---------|---------|
| `--robot-plan` | Parallel execution tracks with `unblocks` lists |
| `--robot-priority` | Priority misalignment detection with confidence |

**Graph Analysis:**
| Command | Returns |
|---------|---------|
| `--robot-insights` | Full metrics: PageRank, betweenness, HITS, eigenvector, critical path, cycles, k-core, articulation points, slack |
| `--robot-label-health` | Per-label health: `health_level`, `velocity_score`, `staleness`, `blocked_count` |
| `--robot-label-flow` | Cross-label dependency: `flow_matrix`, `dependencies`, `bottleneck_labels` |
| `--robot-label-attention [--attention-limit=N]` | Attention-ranked labels |

**History & Change Tracking:**
| Command | Returns |
|---------|---------|
| `--robot-history` | Bead-to-commit correlations |
| `--robot-diff --diff-since <ref>` | Changes since ref: new/closed/modified issues, cycles |

**Other:**
| Command | Returns |
|---------|---------|
| `--robot-burndown <sprint>` | Sprint burndown, scope changes, at-risk items |
| `--robot-forecast <id\|all>` | ETA predictions with dependency-aware scheduling |
| `--robot-alerts` | Stale issues, blocking cascades, priority mismatches |
| `--robot-suggest` | Hygiene: duplicates, missing deps, label suggestions |
| `--robot-graph [--graph-format=json\|dot\|mermaid]` | Dependency graph export |
| `--export-graph <file.html>` | Interactive HTML visualization |

### Scoping & Filtering

```bash
bv --robot-plan --label backend              # Scope to label's subgraph
bv --robot-insights --as-of HEAD~30          # Historical point-in-time
bv --recipe actionable --robot-plan          # Pre-filter: ready to work
bv --recipe high-impact --robot-triage       # Pre-filter: top PageRank
bv --robot-triage --robot-triage-by-track    # Group by parallel work streams
bv --robot-triage --robot-triage-by-label    # Group by domain
```

### Understanding Robot Output

**All robot JSON includes:**
- `data_hash` — Fingerprint of source beads.jsonl
- `status` — Per-metric state: `computed|approx|timeout|skipped` + elapsed ms
- `as_of` / `as_of_commit` — Present when using `--as-of`

**Two-phase analysis:**
- **Phase 1 (instant):** degree, topo sort, density
- **Phase 2 (async, 500ms timeout):** PageRank, betweenness, HITS, eigenvector, cycles

### jq Quick Reference

```bash
bv --robot-triage | jq '.quick_ref'                        # At-a-glance summary
bv --robot-triage | jq '.recommendations[0]'               # Top recommendation
bv --robot-plan | jq '.plan.summary.highest_impact'        # Best unblock target
bv --robot-insights | jq '.status'                         # Check metric readiness
bv --robot-insights | jq '.Cycles'                         # Circular deps (must fix!)
```

---

## UBS — Ultimate Bug Scanner

**Golden Rule:** `ubs <changed-files>` before every commit. Exit 0 = safe. Exit >0 = fix & re-run.

### Commands

```bash
ubs file.rs file2.rs                    # Specific files (< 1s) — USE THIS
ubs $(git diff --name-only --cached)    # Staged files — before commit
ubs --only=rust,toml src/               # Language filter (3-5x faster)
ubs --ci --fail-on-warning .            # CI mode — before PR
ubs .                                   # Whole project (ignores target/, Cargo.lock)
```

### Output Format

```
⚠️  Category (N errors)
    file.rs:42:5 – Issue description
    💡 Suggested fix
Exit code: 1
```

Parse: `file:line:col` → location | 💡 → how to fix | Exit 0/1 → pass/fail

### Fix Workflow

1. Read finding → category + fix suggestion
2. Navigate `file:line:col` → view context
3. Verify real issue (not false positive)
4. Fix root cause (not symptom)
5. Re-run `ubs <file>` → exit 0
6. Commit

### Bug Severity

- **Critical (always fix):** Memory safety, use-after-free, data races, SQL injection
- **Important (production):** Unwrap panics, resource leaks, overflow checks
- **Contextual (judgment):** TODO/FIXME, println! debugging

---

## ast-grep vs ripgrep

**Use `ast-grep` when structure matters.** It parses code and matches AST nodes, ignoring comments/strings, and can **safely rewrite** code.

- Refactors/codemods: rename APIs, change import forms
- Policy checks: enforce patterns across a repo
- Editor/automation: LSP mode, `--json` output

**Use `ripgrep` when text is enough.** Fastest way to grep literals/regex.

- Recon: find strings, TODOs, log lines, config values
- Pre-filter: narrow candidate files before ast-grep

### Rule of Thumb

- Need correctness or **applying changes** → `ast-grep`
- Need raw speed or **hunting text** → `rg`
- Often combine: `rg` to shortlist files, then `ast-grep` to match/modify

### Rust Examples

```bash
# Find structured code (ignores comments)
ast-grep run -l Rust -p 'fn $NAME($$$ARGS) -> $RET { $$$BODY }'

# Find all unwrap() calls
ast-grep run -l Rust -p '$EXPR.unwrap()'

# Quick textual hunt
rg -n 'println!' -t rust

# Combine speed + precision
rg -l -t rust 'unwrap\(' | xargs ast-grep run -l Rust -p '$X.unwrap()' --json
```

---

## Morph Warp Grep — AI-Powered Code Search

**Use `mcp__morph-mcp__warp_grep` for exploratory "how does X work?" questions.** An AI agent expands your query, greps the codebase, reads relevant files, and returns precise line ranges with full context.

**Use `ripgrep` for targeted searches.** When you know exactly what you're looking for.

**Use `ast-grep` for structural patterns.** When you need AST precision for matching/rewriting.

### When to Use What

| Scenario | Tool | Why |
|----------|------|-----|
| "How is pattern matching implemented?" | `warp_grep` | Exploratory; don't know where to start |
| "Where is the quick reject filter?" | `warp_grep` | Need to understand architecture |
| "Find all uses of `Regex::new`" | `ripgrep` | Targeted literal search |
| "Find files with `println!`" | `ripgrep` | Simple pattern |
| "Replace all `unwrap()` with `expect()`" | `ast-grep` | Structural refactor |

### warp_grep Usage

```
mcp__morph-mcp__warp_grep(
  repoPath: "/data/projects/mcp_agent_mail_rust",
  query: "How does the file reservation system work?"
)
```

Returns structured results with file paths, line ranges, and extracted code snippets.

### Anti-Patterns

- **Don't** use `warp_grep` to find a specific function name → use `ripgrep`
- **Don't** use `ripgrep` to understand "how does X work" → wastes time with manual reads
- **Don't** use `ripgrep` for codemods → risks collateral edits

<!-- bv-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

**Important:** `br` is non-invasive—it NEVER executes git commands. After `br sync --flush-only`, you must manually run `git add .beads/ && git commit`.

### Essential Commands

```bash
# View issues (launches TUI - avoid in automated sessions)
bv

# CLI commands for agents (use these instead)
br ready              # Show issues ready to work (no blockers)
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br create --title="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason "Completed"
br close <id1> <id2>  # Close multiple issues at once
br sync --flush-only  # Export to JSONL (NO git operations)
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Run `br sync --flush-only` then manually commit

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers, not words)
- **Types**: task, bug, feature, epic, question, docs
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads to JSONL
git add .beads/         # Stage beads changes
git commit -m "..."     # Commit everything together
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress → closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always `br sync --flush-only && git add .beads/` before ending session

<!-- end-bv-agent-instructions -->

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   br sync --flush-only    # Export beads to JSONL (no git ops)
   git add .beads/         # Stage beads changes
   git add <other files>   # Stage code changes
   git commit -m "..."     # Commit everything
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds


---

Note for Codex/GPT-5.2:

You constantly bother me and stop working with concerned questions that look similar to this:

```
Unexpected changes (need guidance)

- Working tree still shows edits I did not make in Cargo.toml, Cargo.lock, src/cli/commands/upgrade.rs, src/storage/sqlite.rs, tests/conformance.rs, tests/storage_deps.rs. Please advise whether to keep/commit/revert these before any further work. I did not touch them.

Next steps (pick one)

1. Decide how to handle the unrelated modified files above so we can resume cleanly.
2. Triage beads_rust-orko (clippy/cargo warnings) and beads_rust-ydqr (rustfmt failures).
3. If you want a full suite run later, fix conformance/clippy blockers and re‑run cargo test --all.
```

NEVER EVER DO THAT AGAIN. The answer is literally ALWAYS the same: those are changes created by the potentially dozen of other agents working on the project at the same time. This is not only a common occurence, it happens multiple times PER MINUTE. The way to deal with it is simple: you NEVER, under ANY CIRCUMSTANCE, stash, revert, overwrite, or otherwise disturb in ANY way the work of other agents. Just treat those changes identically to changes that you yourself made. Just fool yourself into thinking YOU made the changes and simply don't recall it for some reason.

---

## Note on Built-in TODO Functionality

Also, if I ask you to explicitly use your built-in TODO functionality, don't complain about this and say you need to use beads. You can use built-in TODOs if I tell you specifically to do so. Always comply with such orders.
