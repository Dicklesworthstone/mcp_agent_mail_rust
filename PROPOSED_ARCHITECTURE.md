# Proposed Rust Architecture for MCP Agent Mail

## Goals
- Full feature parity with legacy Python implementation.
- Use required crates: fastmcp_rust, sqlmodel_rust, asupersync, frankentui, beads_rust, coding_agent_session_search.
- Zero tokio usage.
- Git-backed archive remains the source of truth; SQLite is the index layer.

## Workspace Layout (Proposed)
- `crates/mcp-agent-mail-core`
- `crates/mcp-agent-mail-storage`
- `crates/mcp-agent-mail-db`
- `crates/mcp-agent-mail-server`
- `crates/mcp-agent-mail-cli`
- `crates/mcp-agent-mail-guard`
- `crates/mcp-agent-mail-share`
- `crates/mcp-agent-mail-conformance`

## Dependency Mapping
- MCP server + transport: `fastmcp`, `fastmcp-server`, `fastmcp-transport`.
- Async runtime + networking: `asupersync` (structured concurrency, IO primitives, timers).
- SQLite access: `sqlmodel_rust` (schema + queries + migrations).
- TUI/console: `frankentui` (all terminal output).
- Beads integration: `beads_rust` for task metadata and workflows.
- Agent detection: `coding_agent_session_search`.

## Core Components
1. Server (FastMCP)
- Define MCP tools and resources using fastmcp_rust APIs.
- Provide macro helpers matching legacy `macro_*` tools.
- Resource URIs match legacy (`resource://inbox/...`, `resource://thread/...`, etc.).

2. Storage (Git Archive)
- Git repo as source-of-truth for messages, attachments, and reservations.
- Per-project archive root: `projects/<slug>/`.
- Archive lock with `.archive.lock` + advisory file locks.
- Attachment pipeline: convert to WebP, keep originals optionally, manifest + audit.

3. SQLite Index
- Schema mirrors legacy SQLModel tables.
- FTS for message search.
- Query tracking and slow-query logging.
- Concurrency controls and retry semantics.

4. CLI
- Typer CLI parity via Rust CLI (likely clap) but output matches legacy.
- Commands: server, config, guard, share, acks, file reservations, doctor.

5. Guard
- Pre-commit guard install/uninstall with per-project reservations.

6. Share/Export
- Static export bundle generation and verification.
- Optional signing and encryption support.

## Data Flow (High-Level)
- Incoming tool call -> validate -> archive write -> SQLite index update -> response.
- Resource read -> SQLite index (fast path) -> archive fallback (slow path).

## Concurrency: Lock Ordering (Deadlock Guard)

At 1000+ concurrent agents, a single inconsistent lock acquisition order can deadlock the
entire process. We therefore define a **global lock hierarchy** in
`crates/mcp-agent-mail-core/src/lock_order.rs` and (in `debug_assertions` builds) enforce it
at runtime.

### Global Lock Hierarchy (Outer → Inner)

Lock ranks must be acquired strictly in ascending order when nested:

1. `DbPoolCache`
2. `DbReadCache*` (projects/agents caches + deferred touches)
3. `DbQueryTrackerInner`
4. `StorageRepoCache`
5. `StorageSignalDebounce`
6. `StorageWbqDrainHandle`
7. `StorageWbqStats`
8. `StorageCommitQueue`
9. `ToolsBridgedEnv`
10. `ToolsToolMetrics`

### Rules

- Never hold these locks across blocking IO or `.await`.
- Keep critical sections tiny (copy/clone what you need, then drop the guard).
- If you add a new process-global lock:
  - Assign it a unique `LockLevel` rank.
  - Use `OrderedMutex` / `OrderedRwLock` so debug builds panic on ordering violations.
  - Add a focused unit test that would have caught the ordering bug.

## Conformance Harness
- Fixture-based tests comparing Rust outputs to Python reference.
- Separate Python fixture generator living under `tests/conformance/python_reference/`.
- Rust tests load fixtures and assert parity.

## Benchmark Harness
- Benchmarks reuse conformance fixtures as inputs.
- Track latency for send_message, search_messages, archive write.

## Open Questions
- Exact API surface from fastmcp_rust needed for resource rendering.
- sqlmodel_rust query ergonomics for FTS and custom SQL.
- asupersync integration for HTTP server lifecycle.

---

## Required Crate Integration Details

### 1. fastmcp_rust Integration

**Tool Definition Pattern:**
```rust
use fastmcp::prelude::*;

#[tool(description = "Ensure project exists for the given human key")]
async fn ensure_project(
    ctx: &McpContext,
    human_key: String,
    identity_mode: Option<String>,
) -> McpResult<ProjectResult> {
    ctx.checkpoint()?;  // Cancel-safe checkpoint

    let storage = ctx.get_state::<Storage>("storage")
        .ok_or_else(|| McpError::internal_error("Storage not initialized"))?;

    let project = storage.ensure_project(&human_key, identity_mode.as_deref()).await?;
    Ok(ProjectResult::from(project))
}
```

**Resource Definition Pattern:**
```rust
#[resource(uri = "resource://agents/{project_key}", mime_type = "application/json")]
async fn agents_resource(ctx: &McpContext, project_key: String) -> McpResult<String> {
    ctx.checkpoint()?;
    let storage = ctx.get_state::<Storage>("storage")?;
    let agents = storage.list_agents(&project_key).await?;
    Ok(serde_json::to_string(&agents)?)
}
```

**Server Setup (HTTP-only):**
- Build `Server::new("mcp-agent-mail", version)` with all tools/resources.
- Hand the server to the Streamable HTTP transport in `fastmcp-transport` using `HTTP_HOST/PORT/PATH`.
- No stdio/SSE transport in this project (HTTP-only).

### 2. sqlmodel_rust Integration

**Model Definition:**
```rust
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "projects")]
pub struct Project {
    #[sqlmodel(primary_key, auto_increment)]
    pub id: Option<i64>,
    #[sqlmodel(unique, index)]
    pub slug: String,
    #[sqlmodel(index)]
    pub human_key: String,
    pub created_at: NaiveDateTime,
}
```

**Query Patterns:**
```rust
// Insert
let project = Project { id: None, slug, human_key, created_at };
insert!(project).execute(&cx, &conn).await?;

// Select with filter
let projects = select!(Project)
    .filter(col("slug").eq(&slug))
    .order_by(col("created_at").desc())
    .limit(10)
    .execute(&cx, &conn)
    .await?;

// FTS search
let results = raw_query(&cx, &conn,
    "SELECT m.* FROM messages m JOIN fts_messages f ON m.id = f.message_id WHERE f.fts_messages MATCH ?",
    &[&query]
).await?;
```

**Session Pattern (Unit of Work):**
```rust
let session = Session::new(&pool).await?;
session.add(&new_message);
session.flush(&cx).await?;
session.commit(&cx).await?;
```

### 3. asupersync Integration

**Runtime Context:**
All database and I/O operations use `&Cx` context for:
- Budget-aware timeouts
- Cancellation checkpoints
- Structured concurrency

```rust
async fn send_message(cx: &Cx, storage: &Storage, msg: Message) -> Outcome<MessageId, Error> {
    cx.checkpoint()?;  // Early cancellation check

    // Database write
    let id = storage.insert_message(cx, &msg).await?;

    cx.checkpoint()?;  // Check before Git commit

    // Git archive write
    storage.archive_message(cx, &msg, id).await?;

    Outcome::Ok(id)
}
```

**Outcome Type:**
```rust
pub enum Outcome<T, E> {
    Ok(T),           // Success
    Err(E),          // Application error
    Cancelled(Why),  // Cancellation
    Panicked(Msg),   // Panic caught
}
```

### 4. frankentui Integration

**CLI Output:**
```rust
use ftui::prelude::*;

fn print_inbox(messages: &[Message]) {
    let table = Table::new()
        .header(Row::new(["ID", "From", "Subject", "Date"]))
        .rows(messages.iter().map(|m| {
            Row::new([
                m.id.to_string(),
                m.from.clone(),
                m.subject.clone(),
                m.created_ts.format("%Y-%m-%d %H:%M").to_string(),
            ])
        }))
        .theme(TablePresetId::TerminalClassic);

    println!("{}", table.render(80));
}
```

**Progress for Long Operations:**
```rust
let progress = ProgressBar::new(total as f64)
    .label("Exporting messages...");

for (i, msg) in messages.iter().enumerate() {
    progress.set_ratio(i as f64 / total as f64);
    export_message(msg)?;
}
```

### 5. beads_rust Integration

**Issue Tracking for Port Tasks:**
```rust
// Create beads for port tasks
br create --title="Implement send_message tool" --type=task --priority=1

// Track dependencies
br dep add 42 41  // send_message depends on Message model

// Mark complete
br close 42 --reason "Implemented with conformance tests"
```

### 6. coding_agent_session_search Integration

**Agent Detection:**
```rust
use coding_agent_session_search::detect_agent;

fn get_current_agent() -> Option<AgentInfo> {
    detect_agent()
}
```

---

## Error Handling Strategy

**Error Hierarchy:**
```rust
#[derive(Debug, thiserror::Error)]
pub enum MailError {
    #[error("Project not found: {0}")]
    ProjectNotFound(String),

    #[error("Agent not found: {0}")]
    AgentNotFound(String),

    #[error("Contact required: {from} -> {to}")]
    ContactRequired { from: String, to: String },

    #[error("Contact blocked: {from} -> {to}")]
    ContactBlocked { from: String, to: String },

    #[error("File reservation conflict: {pattern}")]
    ReservationConflict { pattern: String, holders: Vec<String> },

    #[error("Database error: {0}")]
    Database(#[from] sqlmodel_rust::Error),

    #[error("Git error: {0}")]
    Git(#[from] git2::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<MailError> for McpError {
    fn from(e: MailError) -> Self {
        match e {
            MailError::ProjectNotFound(s) => McpError::resource_not_found(s),
            MailError::AgentNotFound(s) => McpError::resource_not_found(s),
            MailError::ContactRequired { .. } => McpError::with_data(
                McpErrorCode::Custom(-32005),
                e.to_string(),
                json!({"error_type": "CONTACT_REQUIRED"}),
            ),
            _ => McpError::internal_error(e.to_string()),
        }
    }
}
```

---

*Architecture updated by FuchsiaForge | 2026-02-04*
