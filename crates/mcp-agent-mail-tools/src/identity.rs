//! Identity cluster tools
//!
//! Tools for project and agent identity management:
//! - `health_check`: Infrastructure status
//! - `ensure_project`: Create/ensure project exists
//! - `register_agent`: Register or update agent
//! - `create_agent_identity`: Create new agent identity
//! - whois: Agent profile lookup

use fastmcp::McpErrorCode;
use fastmcp::prelude::*;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbConn, guard_db_conn, micros_to_iso};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::messaging::try_dispatch_archive_write;
use crate::tool_util::{
    db_error_to_mcp_error, db_outcome_to_mcp_result, get_db_pool, get_read_db_pool,
    legacy_tool_error, resolve_project,
};

/// Build recovery status for the health_check response.
///
/// Returns `Some(RecoveryStatusResponse)` when the mailbox is not fully healthy,
/// so operators immediately see the current mode, owner, next action, and bundle path.
/// Returns `None` when the mailbox is healthy (no recovery context to surface).
fn build_recovery_status(config: &Config) -> Option<RecoveryStatusResponse> {
    use mcp_agent_mail_db::mailbox_verdict::{
        DurabilityState, VerdictOptions, compute_mailbox_verdict,
    };
    use mcp_agent_mail_db::pool::{
        MailboxOwnershipDisposition, inspect_mailbox_ownership, inspect_mailbox_recovery_lock,
        resolve_mailbox_sqlite_path,
    };

    let resolved = resolve_mailbox_sqlite_path(&config.database_url).ok()?;
    let db_path = PathBuf::from(&resolved.canonical_path);
    let storage_root = &config.storage_root;

    // Lightweight recovery-lock + ownership probes (no full verdict).
    let recovery_lock = inspect_mailbox_recovery_lock(&db_path);
    let ownership = inspect_mailbox_ownership(&db_path, storage_root.as_path());

    // Compute the full verdict to get the durability state.
    let verdict = compute_mailbox_verdict(
        &config.database_url,
        storage_root.as_path(),
        &VerdictOptions {
            skip_integrity_check: true, // integrity is already probed elsewhere
            ..VerdictOptions::default()
        },
    );
    let durability = DurabilityState::from_mailbox_state(verdict.state);

    if durability == DurabilityState::Healthy && !recovery_lock.active {
        return None;
    }

    let mode = durability.to_string();

    let owner = match ownership.disposition {
        MailboxOwnershipDisposition::Unowned => "none".to_string(),
        MailboxOwnershipDisposition::ActiveOtherOwner => {
            if let Some(proc) = ownership.processes.first() {
                format!("pid {} (active)", proc.pid)
            } else {
                "active (unknown pid)".to_string()
            }
        }
        MailboxOwnershipDisposition::StaleLiveProcess => {
            if let Some(proc) = ownership.processes.first() {
                format!("pid {} (stale)", proc.pid)
            } else {
                "stale (unknown pid)".to_string()
            }
        }
        MailboxOwnershipDisposition::DeletedExecutable => {
            if let Some(proc) = ownership.processes.first() {
                format!("pid {} (deleted executable)", proc.pid)
            } else {
                "deleted executable".to_string()
            }
        }
        MailboxOwnershipDisposition::SplitBrain => format!(
            "split-brain ({} competing pids)",
            ownership.competing_pids.len()
        ),
    };

    let next_action = match durability {
        DurabilityState::Healthy => "No action required".to_string(),
        DurabilityState::DegradedReadOnly => {
            if recovery_lock.active {
                "Recovery in progress; wait for completion or check recovery lock holder"
                    .to_string()
            } else {
                "Run `am doctor repair` to attempt automatic recovery".to_string()
            }
        }
        DurabilityState::Recovering => {
            if let Some(pid) = recovery_lock.pid {
                format!("Recovery active (pid {pid}); wait for completion or investigate stall")
            } else {
                "Recovery lock held but PID unknown; check for stale lock files".to_string()
            }
        }
        DurabilityState::Corrupt => {
            "Run `am doctor repair --yes` or restore from archive backup".to_string()
        }
    };

    // Locate latest forensic bundle if one exists.
    let bundle_path = find_latest_forensic_bundle(storage_root.as_path(), &db_path);

    Some(RecoveryStatusResponse {
        mode,
        owner,
        next_action,
        bundle_path,
        recovery_lock_active: recovery_lock.active,
        recovery_lock_pid: recovery_lock.pid,
    })
}

/// Find the most recently created forensic bundle directory.
fn find_latest_forensic_bundle(
    storage_root: &std::path::Path,
    db_path: &std::path::Path,
) -> Option<String> {
    let forensics_dir = if storage_root.is_dir() {
        storage_root.join("doctor").join("forensics")
    } else {
        db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("doctor")
            .join("forensics")
    };
    if !forensics_dir.is_dir() {
        return None;
    }
    // Walk one level: forensics/<db_family>/<bundle_name>/
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    let Ok(families) = std::fs::read_dir(&forensics_dir) else {
        return None;
    };
    for family_entry in families.flatten() {
        let family_path = family_entry.path();
        if !family_path.is_dir() {
            continue;
        }
        let Ok(bundles) = std::fs::read_dir(&family_path) else {
            continue;
        };
        for bundle_entry in bundles.flatten() {
            let bundle_path = bundle_entry.path();
            if !bundle_path.is_dir() {
                continue;
            }
            let mtime = bundle_entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if latest.as_ref().is_none_or(|(prev, _)| mtime > *prev) {
                latest = Some((mtime, bundle_path));
            }
        }
    }
    latest.map(|(_, path)| path.display().to_string())
}

fn redact_database_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://")
        && let Some((_creds, host)) = rest.rsplit_once('@')
    {
        return format!("{scheme}://****@{host}");
    }
    url.to_string()
}

const fn us_to_ms_ceil(us: u64) -> u64 {
    us.saturating_add(999).saturating_div(1000)
}

const HEALTH_CHECK_SYNC_DB_BUSY_TIMEOUT_MS: u32 = 5_000;
const HEALTH_CHECK_REQUIRED_TABLES: &[&str] = &[
    "projects",
    "agents",
    "messages",
    "message_recipients",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticReadinessResponse {
    pub status: String,
    pub detail: String,
}

fn resolve_health_check_sqlite_path(database_url: &str) -> Result<Option<PathBuf>, String> {
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(database_url) {
        return Ok(None);
    }

    let Some(sqlite_path) =
        mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(database_url)
    else {
        return Err(format!(
            "cannot resolve sqlite path from DATABASE_URL {database_url}"
        ));
    };
    let raw = sqlite_path.to_string_lossy().into_owned();
    let normalized = mcp_agent_mail_db::pool::normalize_sqlite_path_for_pool_key(&raw);
    if normalized != raw {
        return Ok(Some(PathBuf::from(normalized)));
    }

    let relative_path = Path::new(&raw);
    if relative_path.is_absolute() || raw.starts_with("./") || raw.starts_with("../") {
        return Ok(Some(PathBuf::from(raw)));
    }

    if !relative_path.exists() {
        let absolute_candidate = Path::new("/").join(relative_path);
        if absolute_candidate.exists() {
            return Ok(Some(absolute_candidate));
        }
    }

    Ok(Some(PathBuf::from(raw)))
}

fn open_health_check_sync_db_connection(path: &Path) -> Result<DbConn, String> {
    let display = path.display().to_string();
    let conn = DbConn::open_file(&display)
        .map_err(|err| format!("open sqlite file {display} for health_check: {err}"))?;
    conn.execute_raw(&format!(
        "PRAGMA busy_timeout = {HEALTH_CHECK_SYNC_DB_BUSY_TIMEOUT_MS};"
    ))
    .map_err(|err| {
        format!(
            "configure sqlite busy_timeout={HEALTH_CHECK_SYNC_DB_BUSY_TIMEOUT_MS} on {display}: {err}"
        )
    })?;
    Ok(conn)
}

fn semantic_readiness_response(
    status: &str,
    detail: impl Into<String>,
) -> SemanticReadinessResponse {
    SemanticReadinessResponse {
        status: status.to_string(),
        detail: detail.into(),
    }
}

fn health_check_semantic_readiness(config: &Config) -> SemanticReadinessResponse {
    let sqlite_path = match resolve_health_check_sqlite_path(&config.database_url) {
        Ok(path) => path,
        Err(error) => return semantic_readiness_response("fail", error),
    };

    let Some(sqlite_path) = sqlite_path else {
        return semantic_readiness_response(
            "ok",
            "Skipped semantic readiness archive parity for in-memory database",
        );
    };

    if !sqlite_path.exists() {
        return semantic_readiness_response(
            "fail",
            format!(
                "SQLite database is missing at {}; health_check refuses to initialize the mailbox",
                sqlite_path.display()
            ),
        );
    }

    let conn = match open_health_check_sync_db_connection(&sqlite_path) {
        Ok(conn) => conn,
        Err(error) => {
            let status = if mcp_agent_mail_db::is_lock_error(&error) {
                "warn"
            } else {
                "fail"
            };
            return semantic_readiness_response(status, error);
        }
    };
    let conn = guard_db_conn(conn, "identity::health_check semantic probe");

    if let Err(error) = conn.query_sync("SELECT 1", &[]) {
        let error = error.to_string();
        let status = if mcp_agent_mail_db::is_lock_error(&error) {
            "warn"
        } else {
            "fail"
        };
        return semantic_readiness_response(
            status,
            format!("sqlite connectivity probe failed during health_check: {error}"),
        );
    }

    let rows = match conn.query_sync(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        &[],
    ) {
        Ok(rows) => rows,
        Err(error) => {
            return semantic_readiness_response(
                "fail",
                format!("failed to inspect sqlite schema during health_check: {error}"),
            );
        }
    };
    let present = rows
        .into_iter()
        .filter_map(|row| row.get_named::<String>("name").ok())
        .collect::<std::collections::BTreeSet<_>>();
    let missing_tables = HEALTH_CHECK_REQUIRED_TABLES
        .iter()
        .copied()
        .filter(|name| !present.contains(*name))
        .collect::<Vec<_>>();
    if !missing_tables.is_empty() {
        return semantic_readiness_response(
            "fail",
            format!(
                "sqlite schema missing required health_check tables: {}",
                missing_tables.join(", ")
            ),
        );
    }

    let archive = mcp_agent_mail_db::scan_archive_message_inventory(&config.storage_root);
    if archive.projects == 0 && archive.agents == 0 && archive.unique_message_ids == 0 {
        return semantic_readiness_response(
            "ok",
            format!(
                "No canonical archive content found under {}",
                config.storage_root.join("projects").display()
            ),
        );
    }

    let rows = match conn.query_sync(
        "SELECT \
            (SELECT COUNT(*) FROM projects) AS project_count, \
            (SELECT COUNT(*) FROM agents) AS agent_count, \
            (SELECT COUNT(*) FROM messages) AS message_count, \
            COALESCE((SELECT MAX(id) FROM messages), 0) AS max_id",
        &[],
    ) {
        Ok(rows) => rows,
        Err(error) => {
            return semantic_readiness_response(
                "fail",
                format!("failed to inspect sqlite inventory during health_check: {error}"),
            );
        }
    };
    let Some(row) = rows.first() else {
        return semantic_readiness_response(
            "fail",
            "sqlite inventory query returned no rows during health_check",
        );
    };
    let db_project_count = row
        .get_named::<i64>("project_count")
        .ok()
        .and_then(|count| u64::try_from(count).ok())
        .unwrap_or(0);
    let db_agent_count = row
        .get_named::<i64>("agent_count")
        .ok()
        .and_then(|count| u64::try_from(count).ok())
        .unwrap_or(0);
    let db_message_count = row
        .get_named::<i64>("message_count")
        .ok()
        .and_then(|count| u64::try_from(count).ok())
        .unwrap_or(0);
    let db_max_id = row.get_named::<i64>("max_id").unwrap_or(0);
    let archive_max_id = archive.latest_message_id.unwrap_or(0);
    let db_project_identities = match mcp_agent_mail_db::collect_db_project_identities(&conn) {
        Ok(identities) => identities,
        Err(error) => {
            return semantic_readiness_response(
                "fail",
                format!("failed to inspect sqlite project identities during health_check: {error}"),
            );
        }
    };
    let missing_archive_projects =
        mcp_agent_mail_db::archive_missing_project_identities(&archive, &db_project_identities);

    if u64::try_from(archive.projects).unwrap_or(u64::MAX) > db_project_count
        || u64::try_from(archive.agents).unwrap_or(u64::MAX) > db_agent_count
        || u64::try_from(archive.unique_message_ids).unwrap_or(u64::MAX) > db_message_count
        || archive_max_id > db_max_id
        || !missing_archive_projects.is_empty()
    {
        let missing_project_suffix = if missing_archive_projects.is_empty() {
            String::new()
        } else {
            format!(
                ", missing archive project(s) in db: {}",
                missing_archive_projects.join(", ")
            )
        };
        return semantic_readiness_response(
            "fail",
            format!(
                "archive inventory is ahead of the sqlite index (archive projects={}, agents={}, messages={}, latest_id={}, db projects={}, agents={}, messages={}, max_id={}{})",
                archive.projects,
                archive.agents,
                archive.unique_message_ids,
                archive_max_id,
                db_project_count,
                db_agent_count,
                db_message_count,
                db_max_id,
                missing_project_suffix
            ),
        );
    }

    semantic_readiness_response(
        "ok",
        format!(
            "Archive and sqlite inventory are aligned enough for health_check: archive projects={}, agents={}, messages={}, db projects={}, agents={}, messages={}",
            archive.projects,
            archive.agents,
            archive.unique_message_ids,
            db_project_count,
            db_agent_count,
            db_message_count
        ),
    )
}

/// Try to write an agent profile to the git archive. Failures are logged
/// but do not fail the tool call – the DB is the source of truth.
///
/// Uses the write-behind queue when available. If the queue is unavailable,
/// falls back to the direct storage path before giving up.
fn try_write_agent_profile(config: &Config, project_slug: &str, agent_json: &serde_json::Value) {
    let op = mcp_agent_mail_storage::WriteOp::AgentProfile {
        project_slug: project_slug.to_string(),
        config: config.clone(),
        agent_json: agent_json.clone(),
    };
    try_dispatch_archive_write(
        op,
        &format!("agent profile archive write project={project_slug}"),
    );
}

/// If the project root is ephemeral and the current storage root is the
/// default global mailbox, compute an isolated storage root and return a
/// rerouted clone of `config`.  Returns `None` when no reroute is needed
/// (production path, or operator already set a custom `STORAGE_ROOT`).
fn maybe_reroute_ephemeral_storage(config: &Config, human_key: &str) -> Option<Config> {
    let isolated =
        mcp_agent_mail_core::config::compute_ephemeral_storage_root(Path::new(human_key), config)?;

    tracing::info!(
        human_key,
        isolated_root = %isolated.display(),
        "Auto-rerouting ephemeral project to isolated storage root",
    );

    let mut rerouted = config.clone();
    rerouted.storage_root = isolated;
    Some(rerouted)
}

fn enqueue_project_semantic_index(project: &mcp_agent_mail_db::ProjectRow) {
    let project_id = project.id.unwrap_or(0);
    let _ = mcp_agent_mail_db::search_service::enqueue_semantic_document(
        mcp_agent_mail_db::search_planner::DocKind::Project,
        project_id,
        Some(project_id),
        &project.slug,
        &project.human_key,
    );
}

fn enqueue_agent_semantic_index(agent: &mcp_agent_mail_db::AgentRow) {
    let _ = mcp_agent_mail_db::search_service::enqueue_semantic_document(
        mcp_agent_mail_db::search_planner::DocKind::Agent,
        agent.id.unwrap_or(0),
        Some(agent.project_id),
        &agent.name,
        &format!(
            "{}\n{}\n{}",
            agent.program, agent.model, agent.task_description
        ),
    );
}

/// Health check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResponse {
    pub status: String,
    pub health_level: String,
    pub environment: String,
    pub http_host: String,
    pub http_port: u16,
    pub database_url: String,
    pub semantic_readiness: SemanticReadinessResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_utilization: Option<PoolUtilizationResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queues: Option<QueuesHealthResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskHealthResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity: Option<IntegrityHealthResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_indexing: Option<mcp_agent_mail_db::search_service::SemanticIndexingHealth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub two_tier_indexing: Option<mcp_agent_mail_db::search_service::TwoTierIndexingHealth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryStatusResponse>,
}

/// Active recovery state surfaced in health_check when the mailbox is degraded or recovering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryStatusResponse {
    /// Current durability mode: "healthy", "degraded_read_only", "recovering", or "corrupt".
    pub mode: String,
    /// Mailbox ownership disposition: who holds the lock.
    pub owner: String,
    /// Next recommended operator action.
    pub next_action: String,
    /// Path to the most recent forensic bundle, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<String>,
    /// Whether a recovery lock is currently held.
    pub recovery_lock_active: bool,
    /// PID of the process holding the recovery lock, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_lock_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityHealthResponse {
    pub last_ok_ts: i64,
    pub last_check_ts: i64,
    pub checks_total: u64,
    pub failures_total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskHealthResponse {
    pub storage_root: String,
    pub storage_probe_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_probe_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_free_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_free_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_free_bytes: Option<u64>,
    pub pressure: String,
    pub archive_writes_disabled: bool,
    pub warning_threshold_mb: u64,
    pub critical_threshold_mb: u64,
    pub fatal_threshold_mb: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolUtilizationResponse {
    pub active: u64,
    pub idle: u64,
    pub total: u64,
    pub pending: u64,
    pub peak_active: u64,
    pub utilization_pct: u64,
    pub acquire_p50_ms: u64,
    pub acquire_p95_ms: u64,
    pub acquire_p99_ms: u64,
    pub over_80_for_s: u64,
    pub warning: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuesHealthResponse {
    pub wbq: WbqQueueHealthResponse,
    pub commit_coalescer: CommitCoalescerHealthResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WbqQueueHealthResponse {
    pub depth: u64,
    pub capacity: u64,
    pub utilization_pct: u64,
    pub peak_depth: u64,
    pub enqueued_total: u64,
    pub drained_total: u64,
    pub errors_total: u64,
    pub backpressure_total: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub latency_p99_ms: u64,
    pub over_80_for_s: u64,
    pub warning: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitCoalescerHealthResponse {
    pub pending_requests: u64,
    pub soft_cap: u64,
    pub utilization_pct: u64,
    pub peak_pending_requests: u64,
    pub enqueued_total: u64,
    pub drained_total: u64,
    pub errors_total: u64,
    pub sync_fallbacks_total: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub latency_p99_ms: u64,
    pub over_80_for_s: u64,
    pub warning: bool,
}

/// Project response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectResponse {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
    pub created_at: String,
}

/// Project response with worktree identity metadata (when `WORKTREES_ENABLED=1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectWithIdentityResponse {
    pub id: i64,
    pub created_at: String,
    #[serde(flatten)]
    pub identity: mcp_agent_mail_core::ProjectIdentity,
}

/// Default capabilities granted to every registered agent regardless of
/// transport or auth method (Bearer token, JWT, or unauthenticated local).
pub const DEFAULT_AGENT_CAPABILITIES: &[&str] = &[
    "send_message",
    "fetch_inbox",
    "file_reservation_paths",
    "acknowledge_message",
];

/// Agent response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub id: i64,
    pub name: String,
    pub program: String,
    pub model: String,
    pub task_description: String,
    pub inception_ts: String,
    pub last_active_ts: String,
    pub project_id: i64,
    pub attachments_policy: String,
    #[serde(default)]
    pub reaper_exempt: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Registration token for sender identity verification.
    /// Returned only on registration; callers must store it and present it as
    /// `sender_token` when sending messages to prove ownership of this agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_token: Option<String>,
}

/// Whois response with optional recent commits
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoisResponse {
    #[serde(flatten)]
    pub agent: AgentResponse,
    pub recent_commits: Vec<CommitInfo>,
}

/// Git commit information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub hexsha: String,
    pub summary: String,
    pub authored_ts: String,
}

/// Check infrastructure health and return configuration status.
///
/// Returns basic server configuration and status information.
#[tool(description = "Return basic readiness information for the Agent Mail server.")]
#[allow(clippy::too_many_lines)]
pub fn health_check(_ctx: &McpContext) -> McpResult<String> {
    let config = &Config::get();
    let mut semantic_readiness = health_check_semantic_readiness(config);
    let pool = if semantic_readiness.status == "fail" {
        None
    } else {
        match get_db_pool() {
            Ok(pool) => {
                pool.sample_pool_stats_now();
                Some(pool)
            }
            Err(error) => {
                semantic_readiness = semantic_readiness_response(
                    "fail",
                    format!("database pool bootstrap failed during health_check: {error}"),
                );
                None
            }
        }
    };
    // Ensure background workers are running so health_check reports stable
    // queue capacity/soft-cap values even before the first write/commit.
    mcp_agent_mail_storage::wbq_start();
    let _ = mcp_agent_mail_storage::get_commit_coalescer();
    let metrics = mcp_agent_mail_core::global_metrics().snapshot();
    let disk_sample = if config.disk_space_monitor_enabled {
        Some(mcp_agent_mail_core::disk::sample_and_record(config))
    } else {
        None
    };

    let now_us = u64::try_from(mcp_agent_mail_db::now_micros()).unwrap_or(0);
    let over_80_for_s = if metrics.db.pool_over_80_since_us == 0 {
        0
    } else {
        now_us
            .saturating_sub(metrics.db.pool_over_80_since_us)
            .saturating_div(1_000_000)
    };

    // Refresh the cached health level from live metrics
    let (health_level, _changed) = mcp_agent_mail_core::refresh_health_level();

    let response = HealthCheckResponse {
        status: if semantic_readiness.status == "fail" {
            "error".to_string()
        } else {
            "ok".to_string()
        },
        health_level: health_level.to_string(),
        environment: config.app_environment.to_string(),
        http_host: config.http_host.clone(),
        http_port: config.http_port,
        database_url: redact_database_url(&config.database_url),
        semantic_readiness,
        pool_utilization: pool.as_ref().map(|_| PoolUtilizationResponse {
            active: metrics.db.pool_active_connections,
            idle: metrics.db.pool_idle_connections,
            total: metrics.db.pool_total_connections,
            pending: metrics.db.pool_pending_requests,
            peak_active: metrics.db.pool_peak_active_connections,
            utilization_pct: metrics.db.pool_utilization_pct,
            acquire_p50_ms: us_to_ms_ceil(metrics.db.pool_acquire_latency_us.p50),
            acquire_p95_ms: us_to_ms_ceil(metrics.db.pool_acquire_latency_us.p95),
            acquire_p99_ms: us_to_ms_ceil(metrics.db.pool_acquire_latency_us.p99),
            over_80_for_s,
            warning: over_80_for_s >= 300,
        }),
        queues: Some({
            let wbq_over_80_for_s = if metrics.storage.wbq_over_80_since_us == 0 {
                0
            } else {
                now_us
                    .saturating_sub(metrics.storage.wbq_over_80_since_us)
                    .saturating_div(1_000_000)
            };
            let wbq_utilization_pct = if metrics.storage.wbq_capacity == 0 {
                0
            } else {
                metrics
                    .storage
                    .wbq_depth
                    .saturating_mul(100)
                    .saturating_div(metrics.storage.wbq_capacity)
            };

            let commit_over_80_for_s = if metrics.storage.commit_over_80_since_us == 0 {
                0
            } else {
                now_us
                    .saturating_sub(metrics.storage.commit_over_80_since_us)
                    .saturating_div(1_000_000)
            };
            let commit_utilization_pct = if metrics.storage.commit_soft_cap == 0 {
                0
            } else {
                metrics
                    .storage
                    .commit_pending_requests
                    .saturating_mul(100)
                    .saturating_div(metrics.storage.commit_soft_cap)
            };

            QueuesHealthResponse {
                wbq: WbqQueueHealthResponse {
                    depth: metrics.storage.wbq_depth,
                    capacity: metrics.storage.wbq_capacity,
                    utilization_pct: wbq_utilization_pct,
                    peak_depth: metrics.storage.wbq_peak_depth,
                    enqueued_total: metrics.storage.wbq_enqueued_total,
                    drained_total: metrics.storage.wbq_drained_total,
                    errors_total: metrics.storage.wbq_errors_total,
                    backpressure_total: metrics.storage.wbq_fallbacks_total,
                    latency_p50_ms: us_to_ms_ceil(metrics.storage.wbq_queue_latency_us.p50),
                    latency_p95_ms: us_to_ms_ceil(metrics.storage.wbq_queue_latency_us.p95),
                    latency_p99_ms: us_to_ms_ceil(metrics.storage.wbq_queue_latency_us.p99),
                    over_80_for_s: wbq_over_80_for_s,
                    warning: wbq_over_80_for_s >= 300,
                },
                commit_coalescer: CommitCoalescerHealthResponse {
                    pending_requests: metrics.storage.commit_pending_requests,
                    soft_cap: metrics.storage.commit_soft_cap,
                    utilization_pct: commit_utilization_pct,
                    peak_pending_requests: metrics.storage.commit_peak_pending_requests,
                    enqueued_total: metrics.storage.commit_enqueued_total,
                    drained_total: metrics.storage.commit_drained_total,
                    errors_total: metrics.storage.commit_errors_total,
                    sync_fallbacks_total: metrics.storage.commit_sync_fallbacks_total,
                    latency_p50_ms: us_to_ms_ceil(metrics.storage.commit_queue_latency_us.p50),
                    latency_p95_ms: us_to_ms_ceil(metrics.storage.commit_queue_latency_us.p95),
                    latency_p99_ms: us_to_ms_ceil(metrics.storage.commit_queue_latency_us.p99),
                    over_80_for_s: commit_over_80_for_s,
                    warning: commit_over_80_for_s >= 300,
                },
            }
        }),
        disk: disk_sample.as_ref().map(|s| DiskHealthResponse {
            storage_root: config.storage_root.display().to_string(),
            storage_probe_path: s.storage_probe_path.display().to_string(),
            db_probe_path: s.db_probe_path.as_ref().map(|p| p.display().to_string()),
            storage_free_bytes: s.storage_free_bytes,
            db_free_bytes: s.db_free_bytes,
            effective_free_bytes: s.effective_free_bytes,
            pressure: s.pressure.label().to_string(),
            archive_writes_disabled: matches!(
                s.pressure,
                mcp_agent_mail_core::disk::DiskPressure::Critical
                    | mcp_agent_mail_core::disk::DiskPressure::Fatal
            ),
            warning_threshold_mb: config.disk_space_warning_mb,
            critical_threshold_mb: config.disk_space_critical_mb,
            fatal_threshold_mb: config.disk_space_fatal_mb,
            errors: s.errors.clone(),
        }),
        integrity: {
            let im = mcp_agent_mail_db::integrity_metrics();
            if im.checks_total > 0 {
                Some(IntegrityHealthResponse {
                    last_ok_ts: im.last_ok_ts,
                    last_check_ts: im.last_check_ts,
                    checks_total: im.checks_total,
                    failures_total: im.failures_total,
                })
            } else {
                None
            }
        },
        semantic_indexing: mcp_agent_mail_db::search_service::semantic_indexing_health(),
        two_tier_indexing: mcp_agent_mail_db::search_service::two_tier_indexing_health(),
        recovery: build_recovery_status(config),
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Idempotently create or ensure a project exists.
///
/// # Parameters
/// - `human_key`: Absolute path to the project directory (REQUIRED)
/// - `identity_mode`: Optional override for project identity resolution
///
/// # Returns
/// Project descriptor with id, slug, `human_key`, `created_at`
#[tool(
    description = "Idempotently create or ensure a project exists for the given human key.\n\nWhen to use\n-----------\n- First call in a workflow targeting a new repo/path identifier.\n- As a guard before registering agents or sending messages.\n\nHow it works\n------------\n- Validates that `human_key` is an absolute directory path (the agent's working directory).\n- Computes a stable slug from `human_key` (lowercased, safe characters) so\n  multiple agents can refer to the same project consistently.\n- Ensures DB row exists and that the on-disk archive is initialized\n  (e.g., `messages/`, `agents/`, `file_reservations/` directories).\n\nCRITICAL: Project Identity Rules\n---------------------------------\n- The `human_key` MUST be the absolute path to the agent's working directory\n- Two agents working in the SAME directory path are working on the SAME project\n- Example: Both agents in /data/projects/smartedgar_mcp \u{2192} SAME project\n- Sibling projects are DIFFERENT directories (e.g., /data/projects/smartedgar_mcp\n  vs /data/projects/smartedgar_mcp_frontend)\n\nParameters\n----------\nhuman_key : str\n    The absolute path to the agent's working directory (e.g., \"/data/projects/backend\").\n    This MUST be an absolute path, not a relative path or arbitrary slug.\n    This is the canonical identifier for the project - all agents working in this\n    directory will share the same project identity.\n\nReturns\n-------\ndict\n    Minimal project descriptor: { id, slug, human_key, created_at }.\n\nExamples\n--------\nJSON-RPC:\n```json\n{\n  \"jsonrpc\": \"2.0\",\n  \"id\": \"2\",\n  \"method\": \"tools/call\",\n  \"params\": {\"name\": \"ensure_project\", \"arguments\": {\"human_key\": \"/data/projects/backend\"}}\n}\n```\n\nCommon mistakes\n---------------\n- Passing a relative path (e.g., \"./backend\") instead of an absolute path\n- Using arbitrary slugs instead of the actual working directory path\n- Creating separate projects for the same directory with different slugs\n\nIdempotency\n-----------\n- Safe to call multiple times. If the project already exists, the existing\n  record is returned and the archive is ensured on disk (no destructive changes)."
)]
pub async fn ensure_project(
    ctx: &McpContext,
    human_key: String,
    identity_mode: Option<String>,
) -> McpResult<String> {
    if !Path::new(&human_key).is_absolute() {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Invalid argument value: human_key must be an absolute directory path, got: '{human_key}'. \
Use the agent's working directory path (e.g., '/data/projects/backend' on Unix or 'C:\\\\projects\\\\backend' on Windows). \
Check that all parameters have valid values."
            ),
            true,
            json!({
                "field": "human_key",
                "error_detail": human_key,
            }),
        ));
    }

    let base_config = Config::get();
    let config = match maybe_reroute_ephemeral_storage(&base_config, &human_key) {
        Some(rerouted) => rerouted,
        None => base_config,
    };
    let config = &config;
    let pool = get_db_pool()?;

    // Log identity_mode if provided (future: resolve project identity via git remotes, etc.)
    if let Some(mode) = identity_mode {
        tracing::debug!("ensure_project identity_mode={mode}");
    }

    let row = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::ensure_project(ctx.cx(), &pool, &human_key).await,
    )?;
    enqueue_project_semantic_index(&row);

    // Ensure the git archive directory exists for this project and persist
    // canonical project metadata for DB reconstruction.
    match mcp_agent_mail_storage::ensure_archive(config, &row.slug) {
        Ok(archive) => {
            if let Err(e) = mcp_agent_mail_storage::write_project_metadata_with_config(
                &archive,
                config,
                &row.human_key,
            ) {
                tracing::warn!(
                    "Failed to persist project metadata for project '{}': {e}",
                    row.slug
                );
            }
        }
        Err(e) => {
            tracing::warn!("Failed to ensure archive for project '{}': {e}", row.slug);
        }
    }

    // Always return extended format with identity fields (null when not resolved)
    let mut identity = mcp_agent_mail_core::resolve_project_identity(&human_key);
    identity.slug.clone_from(&row.slug);

    let response = ProjectWithIdentityResponse {
        id: row.id.unwrap_or(0),
        created_at: micros_to_iso(row.created_at),
        identity,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Register or update an agent identity within a project.
///
/// # Parameters
/// - `project_key`: Project human key or slug
/// - `program`: Agent program (e.g., "claude-code", "codex-cli")
/// - `model`: Model identifier (e.g., "opus-4.5", "gpt5-codex")
/// - `name`: Optional agent name (auto-generated if omitted)
/// - `task_description`: Optional current task description
/// - `attachments_policy`: Optional attachment handling policy
/// - `reaper_exempt`: Optional bool to exempt agent from the inactivity reaper (default: false)
///
/// # Returns
/// Agent profile with all fields
#[allow(clippy::too_many_lines)]
#[expect(
    clippy::too_many_arguments,
    reason = "MCP tool signatures mirror the public JSON-RPC schema"
)]
#[tool(
    description = "Create or update an agent identity within a project and persist its profile to Git.\n\nWhen to use\n-----------\n- At the start of a coding session by any automated agent.\n- To update an existing agent's program/model/task metadata and bump last_active.\n\nSemantics\n---------\n- If `name` is omitted, a random adjective+noun name is auto-generated.\n- Reusing the same `name` updates the profile (program/model/task) and\n  refreshes `last_active_ts`.\n- A `profile.json` file is written under `agents/<Name>/` in the project archive.\n\nCRITICAL: Agent Naming Rules\n-----------------------------\n- Agent names MUST be randomly generated adjective+noun combinations\n- Examples: \"GreenLake\", \"BlueDog\", \"RedStone\", \"PurpleBear\"\n- Names should be unique, easy to remember, and NOT descriptive\n- INVALID examples: \"BackendHarmonizer\", \"DatabaseMigrator\", \"UIRefactorer\"\n- The whole point: names should be memorable identifiers, not role descriptions\n- Best practice: Omit the `name` parameter to auto-generate a valid name\n\nParameters\n----------\nproject_key : str\n    The same human key you passed to `ensure_project` (or equivalent identifier).\nprogram : str\n    The agent program (e.g., \"codex-cli\", \"claude-code\").\nmodel : str\n    The underlying model (e.g., \"gpt5-codex\", \"opus-4.1\").\nname : Optional[str]\n    MUST be a valid adjective+noun combination if provided (e.g., \"BlueLake\").\n    If omitted, a random valid name is auto-generated (RECOMMENDED).\n    Names are unique per project; passing the same name updates the profile.\ntask_description : str\n    Short description of current focus (shows up in directory listings).\n\nReturns\n-------\ndict\n    { id, name, program, model, task_description, inception_ts, last_active_ts, project_id }\n\nExamples\n--------\nRegister with auto-generated name (RECOMMENDED):\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"3\",\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\n  \"project_key\":\"/data/projects/backend\",\"program\":\"codex-cli\",\"model\":\"gpt5-codex\",\"task_description\":\"Auth refactor\"\n}}}\n```\n\nRegister with explicit valid name:\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"4\",\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\n  \"project_key\":\"/data/projects/backend\",\"program\":\"claude-code\",\"model\":\"opus-4.1\",\"name\":\"BlueLake\",\"task_description\":\"Navbar redesign\"\n}}}\n```\n\nPitfalls\n--------\n- Names MUST match the adjective+noun format or an error will be raised\n- Names are case-insensitive unique. If you see \"already in use\", pick another or omit `name`.\n- Use the same `project_key` consistently across cooperating agents."
)]
pub async fn register_agent(
    ctx: &McpContext,
    project_key: String,
    program: String,
    model: String,
    name: Option<String>,
    task_description: Option<String>,
    attachments_policy: Option<String>,
    reaper_exempt: Option<bool>,
) -> McpResult<String> {
    use mcp_agent_mail_core::models::{detect_agent_name_mistake, generate_agent_name};

    // Validate program and model are non-empty
    let program = program.trim().to_string();
    if program.is_empty() {
        return Err(legacy_tool_error(
            "EMPTY_PROGRAM",
            "program cannot be empty. Provide the name of your AI coding tool \
             (e.g., 'claude-code', 'codex-cli', 'cursor', 'cline').",
            true,
            json!({ "provided": program }),
        ));
    }

    let model = model.trim().to_string();
    if model.is_empty() {
        return Err(legacy_tool_error(
            "EMPTY_MODEL",
            "model cannot be empty. Provide the underlying model identifier \
             (e.g., 'claude-opus-4.5', 'gpt-4-turbo', 'claude-sonnet-4').",
            true,
            json!({ "provided": model }),
        ));
    }

    let pool = get_db_pool()?;

    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Validate or generate agent name
    let agent_name = match name {
        Some(n) => {
            let n = n.trim();
            if n.is_empty() {
                generate_agent_name()
            } else if let Some(normalized) = mcp_agent_mail_core::models::normalize_agent_name(n) {
                normalized
            } else {
                let (err_type, msg) = detect_agent_name_mistake(n).unwrap_or_else(|| {
                    (
                        "INVALID_AGENT_NAME",
                        format!(
                            "Invalid agent name format: '{n}'. Agent names MUST be randomly generated adjective+noun combinations (e.g., 'GreenLake', 'BlueDog'), NOT descriptive names. Omit the 'name' parameter to auto-generate a valid name."
                        ),
                    )
                });
                return Err(legacy_tool_error(
                    err_type,
                    msg,
                    true,
                    json!({ "provided": n }),
                ));
            }
        }
        None => generate_agent_name(),
    };

    // Validate and normalize attachments_policy (case-insensitive, trimmed)
    let raw_policy = attachments_policy.unwrap_or_else(|| "auto".to_string());
    let policy = raw_policy.trim().to_ascii_lowercase();
    if !is_valid_attachments_policy(&policy) {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Invalid argument value: Invalid attachments_policy '{raw_policy}'. \
Must be: auto, inline, file, or none. \
Check that all parameters have valid values."
            ),
            true,
            json!({
                "field": "attachments_policy",
                "error_detail": raw_policy,
            }),
        ));
    }

    let agent_out = mcp_agent_mail_db::queries::register_agent(
        ctx.cx(),
        &pool,
        project_id,
        &agent_name,
        &program,
        &model,
        task_description.as_deref(),
        Some(&policy),
        reaper_exempt,
    )
    .await;

    let mut row = db_outcome_to_mcp_result(agent_out)?;
    enqueue_agent_semantic_index(&row);

    // Generate and persist a registration token for sender identity verification.
    // Every registration (new or update) rotates the token so that only the
    // most recent registrant can prove ownership.
    let mut registration_token = mcp_agent_mail_core::setup::generate_registration_token();
    if let Some(agent_id) = row.id {
        let token_update = mcp_agent_mail_db::queries::update_agent_registration_token(
            ctx.cx(),
            &pool,
            agent_id,
            &registration_token,
        )
        .await;
        if let Err(e) = db_outcome_to_mcp_result(token_update) {
            tracing::warn!(
                "failed to persist registration_token for agent {}: {e}",
                row.name
            );
            // Do not cache or return a token that was not persisted.
            registration_token.clear();
        }
    }

    // Update the in-memory row so the cache receives the freshly persisted token.
    // Without this, resolve_agent would serve a stale AgentRow (token = None)
    // and sender_token verification in send_message would silently downgrade
    // to unverified.  Only set when persistence succeeded.
    if !registration_token.is_empty() {
        row.registration_token = Some(registration_token.clone());
    }

    // Invalidate + repopulate read cache after mutation
    mcp_agent_mail_db::read_cache().invalidate_agent(project_id, &row.name, row.id);
    mcp_agent_mail_db::read_cache().put_agent(&row);

    // Write agent profile to git archive (best-effort)
    let config = &Config::get();
    let agent_json = serde_json::json!({
        "name": row.name,
        "program": row.program,
        "model": row.model,
        "task_description": row.task_description,
        "inception_ts": micros_to_iso(row.inception_ts),
        "last_active_ts": micros_to_iso(row.last_active_ts),
        "attachments_policy": row.attachments_policy,
    });
    try_write_agent_profile(config, &project.slug, &agent_json);

    // Write per-pane identity file (best-effort, only when $TMUX_PANE is set)
    if let Some(result) =
        mcp_agent_mail_core::write_identity_current_pane(&project.human_key, &row.name)
    {
        match result {
            Ok(path) => {
                tracing::debug!("wrote pane identity file: {}", path.display());
            }
            Err(e) => {
                tracing::warn!("failed to write pane identity file: {e}");
            }
        }
    }

    let response = AgentResponse {
        id: row.id.unwrap_or(0),
        name: row.name,
        program: row.program,
        model: row.model,
        task_description: row.task_description,
        inception_ts: micros_to_iso(row.inception_ts),
        last_active_ts: micros_to_iso(row.last_active_ts),
        project_id: row.project_id,
        attachments_policy: row.attachments_policy,
        reaper_exempt: row.reaper_exempt != 0,
        capabilities: DEFAULT_AGENT_CAPABILITIES
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        registration_token: Some(registration_token),
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Create a new, unique agent identity.
///
/// Always creates a new identity with a fresh unique name (never updates existing).
///
/// # Parameters
/// - `project_key`: Project human key or slug
/// - `program`: Agent program
/// - `model`: Model identifier
/// - `name_hint`: Optional name hint (must be valid adjective+noun if provided)
/// - `task_description`: Optional current task description
/// - `attachments_policy`: Optional attachment handling policy
///
/// # Returns
/// New agent profile
#[allow(clippy::too_many_lines)]
#[tool(
    description = "Create a new, unique agent identity and persist its profile to Git.\n\nHow this differs from `register_agent`\n--------------------------------------\n- Always creates a new identity with a fresh unique name (never updates an existing one).\n- `name_hint`, if provided, MUST be a valid adjective+noun combination and must be available,\n  otherwise an error is raised. Without a hint, a random adjective+noun name is generated.\n\nCRITICAL: Agent Naming Rules\n-----------------------------\n- Agent names MUST be randomly generated adjective+noun combinations\n- Examples: \"GreenCastle\", \"BlueLake\", \"RedStone\", \"PurpleBear\"\n- Names should be unique, easy to remember, and NOT descriptive\n- INVALID examples: \"BackendHarmonizer\", \"DatabaseMigrator\", \"UIRefactorer\"\n- Best practice: Omit `name_hint` to auto-generate a valid name (RECOMMENDED)\n\nWhen to use\n-----------\n- Spawning a brand new worker agent that should not overwrite an existing profile.\n- Temporary task-specific identities (e.g., short-lived refactor assistants).\n\nReturns\n-------\ndict\n    { id, name, program, model, task_description, inception_ts, last_active_ts, project_id }\n\nExamples\n--------\nAuto-generate name (RECOMMENDED):\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"c2\",\"method\":\"tools/call\",\"params\":{\"name\":\"create_agent_identity\",\"arguments\":{\n  \"project_key\":\"/data/projects/backend\",\"program\":\"claude-code\",\"model\":\"opus-4.1\"\n}}}\n```\n\nWith valid name hint:\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"c1\",\"method\":\"tools/call\",\"params\":{\"name\":\"create_agent_identity\",\"arguments\":{\n  \"project_key\":\"/data/projects/backend\",\"program\":\"codex-cli\",\"model\":\"gpt5-codex\",\"name_hint\":\"GreenCastle\",\n  \"task_description\":\"DB migration spike\"\n}}}\n```"
)]
pub async fn create_agent_identity(
    ctx: &McpContext,
    project_key: String,
    program: String,
    model: String,
    name_hint: Option<String>,
    task_description: Option<String>,
    attachments_policy: Option<String>,
) -> McpResult<String> {
    use mcp_agent_mail_core::models::{detect_agent_name_mistake, generate_agent_name};

    // Validate program and model are non-empty
    let program = program.trim().to_string();
    if program.is_empty() {
        return Err(legacy_tool_error(
            "EMPTY_PROGRAM",
            "program cannot be empty. Provide the name of your AI coding tool \
             (e.g., 'claude-code', 'codex-cli', 'cursor', 'cline').",
            true,
            json!({ "provided": program }),
        ));
    }

    let model = model.trim().to_string();
    if model.is_empty() {
        return Err(legacy_tool_error(
            "EMPTY_MODEL",
            "model cannot be empty. Provide the underlying model identifier \
             (e.g., 'claude-opus-4.5', 'gpt-4-turbo', 'claude-sonnet-4').",
            true,
            json!({ "provided": model }),
        ));
    }

    let pool = get_db_pool()?;

    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Generate or validate agent name
    let agent_name = match name_hint {
        Some(hint) => {
            let hint = hint.trim();
            if hint.is_empty() {
                generate_agent_name()
            } else if let Some(normalized) = mcp_agent_mail_core::models::normalize_agent_name(hint)
            {
                normalized
            } else {
                let (err_type, msg) = detect_agent_name_mistake(hint).unwrap_or_else(|| {
                    (
                        "INVALID_AGENT_NAME",
                        format!(
                            "Invalid agent name format: '{hint}'. Agent names MUST be randomly generated adjective+noun combinations (e.g., 'GreenLake', 'BlueDog'), NOT descriptive names. Omit the 'name' parameter to auto-generate a valid name."
                        ),
                    )
                });
                return Err(legacy_tool_error(
                    err_type,
                    msg,
                    true,
                    json!({ "provided": hint }),
                ));
            }
        }
        None => generate_agent_name(),
    };

    // Validate and normalize attachments_policy (case-insensitive, trimmed)
    let raw_policy = attachments_policy.unwrap_or_else(|| "auto".to_string());
    let policy = raw_policy.trim().to_ascii_lowercase();
    if !is_valid_attachments_policy(&policy) {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Invalid argument value: Invalid attachments_policy '{raw_policy}'. \
Must be: auto, inline, file, or none. \
Check that all parameters have valid values."
            ),
            true,
            json!({
                "field": "attachments_policy",
                "error_detail": raw_policy,
            }),
        ));
    }

    // Atomic insert-if-absent: eliminates TOCTOU race between a separate
    // get_agent check and register_agent upsert. Returns Duplicate if the
    // name was taken between validation and insert.
    let agent_out = mcp_agent_mail_db::queries::create_agent(
        ctx.cx(),
        &pool,
        project_id,
        &agent_name,
        &program,
        &model,
        task_description.as_deref(),
        Some(&policy),
    )
    .await;

    let mut row = match agent_out {
        Outcome::Ok(row) => row,
        Outcome::Err(mcp_agent_mail_db::DbError::Duplicate { .. }) => {
            return Err(legacy_tool_error(
                "INVALID_ARGUMENT",
                format!(
                    "Invalid argument value: Agent name '{agent_name}' already exists in this project. \
Choose a different name (or omit the name to auto-generate one)."
                ),
                true,
                json!({
                    "field": "name_hint",
                    "error_detail": agent_name,
                }),
            ));
        }
        Outcome::Err(other) => return Err(db_error_to_mcp_error(other)),
        Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
        Outcome::Panicked(p) => {
            return Err(McpError::internal_error(format!(
                "Internal panic: {}",
                p.message()
            )));
        }
    };
    enqueue_agent_semantic_index(&row);

    // Generate and persist a registration token for sender identity verification.
    let mut registration_token = mcp_agent_mail_core::setup::generate_registration_token();
    if let Some(agent_id) = row.id {
        let token_update = mcp_agent_mail_db::queries::update_agent_registration_token(
            ctx.cx(),
            &pool,
            agent_id,
            &registration_token,
        )
        .await;
        if let Err(e) = db_outcome_to_mcp_result(token_update) {
            tracing::warn!(
                "failed to persist registration_token for agent {}: {e}",
                row.name
            );
            // Do not cache or return a token that was not persisted.
            registration_token.clear();
        }
    }

    // Update the in-memory row so the cache receives the freshly persisted token.
    // Without this, resolve_agent would serve a stale AgentRow (token = None)
    // and sender_token verification in send_message would silently downgrade
    // to unverified.  Only set when persistence succeeded.
    if !registration_token.is_empty() {
        row.registration_token = Some(registration_token.clone());
    }

    // Invalidate + repopulate read cache after mutation
    mcp_agent_mail_db::read_cache().invalidate_agent(project_id, &row.name, row.id);
    mcp_agent_mail_db::read_cache().put_agent(&row);

    // Write agent profile to git archive (best-effort)
    let config = &Config::get();
    let agent_json = serde_json::json!({
        "name": row.name,
        "program": row.program,
        "model": row.model,
        "task_description": row.task_description,
        "inception_ts": micros_to_iso(row.inception_ts),
        "last_active_ts": micros_to_iso(row.last_active_ts),
        "attachments_policy": row.attachments_policy,
    });
    try_write_agent_profile(config, &project.slug, &agent_json);

    // Write per-pane identity file (best-effort, only when $TMUX_PANE is set)
    if let Some(result) =
        mcp_agent_mail_core::write_identity_current_pane(&project.human_key, &row.name)
    {
        match result {
            Ok(path) => {
                tracing::debug!("wrote pane identity file: {}", path.display());
            }
            Err(e) => {
                tracing::warn!("failed to write pane identity file: {e}");
            }
        }
    }

    let response = AgentResponse {
        id: row.id.unwrap_or(0),
        name: row.name,
        program: row.program,
        model: row.model,
        task_description: row.task_description,
        inception_ts: micros_to_iso(row.inception_ts),
        last_active_ts: micros_to_iso(row.last_active_ts),
        project_id: row.project_id,
        attachments_policy: row.attachments_policy,
        reaper_exempt: row.reaper_exempt != 0,
        capabilities: DEFAULT_AGENT_CAPABILITIES
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        registration_token: Some(registration_token),
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Validate `attachments_policy` value.
///
/// Returns `true` if the policy is one of the valid values: auto, inline, file, none.
#[must_use]
pub fn is_valid_attachments_policy(policy: &str) -> bool {
    ["auto", "inline", "file", "none"].contains(&policy)
}

/// Look up agent profile with optional recent commits.
///
/// # Parameters
/// - `project_key`: Project human key or slug
/// - `agent_name`: Agent name to look up
/// - `include_recent_commits`: Include recent Git commits (default: true)
/// - `commit_limit`: Max commits to include (default: 5)
///
/// # Returns
/// Agent profile with optional commit history
#[tool(
    description = "Return enriched profile details for an agent, optionally including recent archive commits.\n\nDiscovery\n---------\nTo discover available agent names, use: resource://agents/{project_key}\nAgent names are NOT the same as program names or user names.\n\nParameters\n----------\nproject_key : str\n    Project slug or human key.\nagent_name : str\n    Agent name to look up (use resource://agents/{project_key} to discover names).\ninclude_recent_commits : bool\n    If true, include latest commits touching the project archive authored by the configured git author.\ncommit_limit : int\n    Maximum number of recent commits to include.\n\nReturns\n-------\ndict\n    Agent profile augmented with { recent_commits: [{hexsha, summary, authored_ts}] } when requested."
)]
pub async fn whois(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    include_recent_commits: Option<bool>,
    commit_limit: Option<u32>,
) -> McpResult<String> {
    let agent_name =
        mcp_agent_mail_core::models::normalize_agent_name(&agent_name).unwrap_or(agent_name);

    let pool = get_read_db_pool()?;

    let include_commits = include_recent_commits.unwrap_or(true);
    let limit_raw = commit_limit.unwrap_or(5);
    let limit = usize::try_from(limit_raw).unwrap_or(0);

    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent_out =
        mcp_agent_mail_db::queries::get_agent(ctx.cx(), &pool, project_id, &agent_name).await;
    let agent_row = match agent_out {
        asupersync::Outcome::Ok(row) => row,
        asupersync::Outcome::Err(mcp_agent_mail_db::DbError::NotFound { .. }) => {
            // Return a user-friendly error without leaking internal project_id.
            return Err(legacy_tool_error(
                "AGENT_NOT_FOUND",
                &format!(
                    "Agent '{agent_name}' not found in project '{}'",
                    project.slug
                ),
                true,
                serde_json::json!({ "agent_name": agent_name, "project": project.slug }),
            ));
        }
        other => db_outcome_to_mcp_result(other)?,
    };

    // Fetch recent commits from the git archive if requested
    let recent_commits = if include_commits && limit > 0 {
        let config = &Config::get();
        match mcp_agent_mail_storage::ensure_archive(config, &project.slug) {
            Ok(archive) => {
                let path_filter = format!("projects/{}/agents/{}", project.slug, agent_row.name);
                match mcp_agent_mail_storage::get_recent_commits(
                    &archive,
                    limit,
                    Some(&path_filter),
                ) {
                    Ok(commits) => commits
                        .into_iter()
                        .map(|c| CommitInfo {
                            hexsha: c.sha,
                            summary: c.summary,
                            authored_ts: c.date,
                        })
                        .collect(),
                    Err(e) => {
                        tracing::warn!("Failed to get recent commits: {e}");
                        Vec::new()
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to ensure archive for commits: {e}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let response = WhoisResponse {
        agent: AgentResponse {
            id: agent_row.id.unwrap_or(0),
            name: agent_row.name,
            program: agent_row.program,
            model: agent_row.model,
            task_description: agent_row.task_description,
            inception_ts: micros_to_iso(agent_row.inception_ts),
            last_active_ts: micros_to_iso(agent_row.last_active_ts),
            project_id: agent_row.project_id,
            attachments_policy: agent_row.attachments_policy,
            reaper_exempt: agent_row.reaper_exempt != 0,
            capabilities: DEFAULT_AGENT_CAPABILITIES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            // Never expose registration_token in whois responses
            registration_token: None,
        },
        recent_commits,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

fn resolve_identity_from_project_keys(
    project_keys: &[String],
    pane_id: &str,
) -> Option<(String, std::path::PathBuf)> {
    project_keys.iter().find_map(|project_key| {
        mcp_agent_mail_core::resolve_identity_with_path(project_key, pane_id)
    })
}

/// Resolve the agent name for a tmux pane from the canonical identity file.
///
/// # Parameters
/// - `project_key`: Absolute path to the project directory
/// - `pane_id`: Optional tmux pane identifier (reads `$TMUX_PANE` if omitted)
///
/// # Returns
/// The agent name if found, or an error if no identity file exists.
#[tool(
    description = "Resolve the agent name for a tmux pane from the canonical per-pane identity file.\n\nChecks the following locations in priority order:\n1. Canonical: ~/.config/agent-mail/identity/<project_hash>/<pane_id>\n2. Legacy Claude Code: ~/.claude/agent-mail/identity.<pane_id>\n3. Legacy NTM: /tmp/agent-mail-name.<project_hash>.<pane_id>\n\nParameters\n----------\nproject_key : str\n    Absolute path to the project directory (used to scope the lookup).\npane_id : Optional[str]\n    Tmux pane identifier (e.g., \"%0\", \"%3\"). If omitted, reads $TMUX_PANE.\n\nReturns\n-------\ndict\n    { agent_name, pane_id, identity_path }"
)]
pub async fn resolve_pane_identity(
    ctx: &McpContext,
    project_key: String,
    pane_id: Option<String>,
) -> McpResult<String> {
    let effective_pane = match pane_id {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => mcp_agent_mail_core::get_composite_tmux_pane_id().unwrap_or_default(),
    };

    if effective_pane.is_empty() {
        return Err(legacy_tool_error(
            "MISSING_PANE_ID",
            "No pane_id provided and $TMUX_PANE is not set. \
             Provide pane_id explicitly or run inside a tmux session.",
            true,
            json!({}),
        ));
    }

    let mut project_keys = vec![project_key.clone()];
    if !Path::new(&project_key).is_absolute()
        && let Ok(pool) = get_read_db_pool()
        && let Ok(project) = resolve_project(ctx, &pool, &project_key).await
        && project.human_key != project_key
    {
        project_keys.push(project.human_key);
    }

    let checked_path = mcp_agent_mail_core::canonical_identity_path(
        project_keys.last().unwrap_or(&project_key),
        &effective_pane,
    );

    resolve_identity_from_project_keys(&project_keys, &effective_pane).map_or_else(
        || {
            Err(legacy_tool_error(
                "IDENTITY_NOT_FOUND",
                format!(
                    "No identity file found for pane '{effective_pane}' in project '{project_key}'. \
                     Register an agent first with register_agent or macro_start_session."
                ),
                false,
                json!({
                    "pane_id": effective_pane,
                    "project_key": project_key,
                    "checked_path": checked_path.to_string_lossy(),
                }),
            ))
        },
        |(agent_name, resolved_path)| {
            let response = json!({
                "agent_name": agent_name,
                "pane_id": effective_pane,
                "identity_path": resolved_path.to_string_lossy(),
            });
            serde_json::to_string(&response)
                .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
        },
    )
}

/// Clean up stale per-pane identity files for dead tmux panes.
///
/// # Parameters
/// - `project_key`: Optional project key to scope cleanup (cleans all projects if omitted)
///
/// # Returns
/// List of removed file paths.
#[tool(
    description = "Remove stale per-pane identity files for tmux panes that no longer exist.\n\nQueries tmux for live panes and removes identity files that reference dead panes.\nSafety: does nothing if tmux is not running (to avoid accidentally removing everything).\n\nParameters\n----------\nproject_key : Optional[str]\n    If provided, only clean up identity files for this project.\n    If omitted, clean up across all projects.\n\nReturns\n-------\ndict\n    { removed_count, removed_paths }"
)]
pub fn cleanup_pane_identities(
    _ctx: &McpContext,
    project_key: Option<String>,
) -> McpResult<String> {
    let removed = project_key
        .map_or_else(mcp_agent_mail_core::cleanup_all_stale_identities, |key| {
            mcp_agent_mail_core::cleanup_stale_identities(&key)
        });

    let paths: Vec<String> = removed
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let response = json!({
        "removed_count": removed.len(),
        "removed_paths": paths,
    });

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// List all registered agents in a project.
///
/// # Parameters
/// - `project_key`: Project slug or human key
///
/// # Returns
/// Array of agent entries with name, role (program), project scope, registration time, last seen
#[tool(
    description = "List all registered agents in a project.\n\nReturns agent name, role (program), model, task description, registration time (inception_ts), and last seen (last_active_ts).\n\nParameters\n----------\nproject_key : str\n    Project slug or human key.\n\nReturns\n-------\nstr (JSON)\n    Array of agent objects with fields: name, program, model, task_description, inception_ts, last_active_ts, contact_policy."
)]
pub async fn list_agents(ctx: &McpContext, project_key: String) -> McpResult<String> {
    let pool = get_read_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agents = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;

    let entries: Vec<serde_json::Value> = agents
        .into_iter()
        .map(|a| {
            json!({
                "name": a.name,
                "program": a.program,
                "model": a.model,
                "task_description": a.task_description,
                "inception_ts": micros_to_iso(a.inception_ts),
                "last_active_ts": micros_to_iso(a.last_active_ts),
                "contact_policy": a.contact_policy,
            })
        })
        .collect();

    serde_json::to_string(&entries)
        .map_err(|e| McpError::internal_error(format!("JSON serialization error: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::Cx;
    use asupersync::runtime::RuntimeBuilder;
    use fastmcp::McpContext;
    use mcp_agent_mail_core::config::with_process_env_overrides_for_test;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    static HEALTH_CHECK_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    // ── redact_database_url ──

    #[test]
    fn redact_hides_password_in_postgres_url() {
        assert_eq!(
            redact_database_url("postgres://user:secret@localhost/db"),
            "postgres://****@localhost/db"
        );
    }

    #[test]
    fn redact_hides_password_in_sqlite_userinfo() {
        assert_eq!(
            redact_database_url("sqlite://admin:pass123@/data/test.db"),
            "sqlite://****@/data/test.db"
        );
    }

    #[test]
    fn redact_preserves_url_without_credentials() {
        assert_eq!(
            redact_database_url("sqlite:///data/agent_mail.db"),
            "sqlite:///data/agent_mail.db"
        );
    }

    #[test]
    fn redact_preserves_plain_path() {
        assert_eq!(
            redact_database_url("/data/agent_mail.db"),
            "/data/agent_mail.db"
        );
    }

    #[test]
    fn redact_handles_empty_string() {
        assert_eq!(redact_database_url(""), "");
    }

    #[test]
    fn redact_handles_no_at_sign() {
        assert_eq!(
            redact_database_url("postgres://localhost/db"),
            "postgres://localhost/db"
        );
    }

    #[test]
    fn redact_handles_complex_password_with_special_chars() {
        assert_eq!(
            redact_database_url("postgres://user:p@ss%40word@host:5432/db"),
            "postgres://****@host:5432/db"
        );
    }

    // ── is_valid_attachments_policy ──

    #[test]
    fn valid_attachments_policies_accepted() {
        assert!(is_valid_attachments_policy("auto"));
        assert!(is_valid_attachments_policy("inline"));
        assert!(is_valid_attachments_policy("file"));
        assert!(is_valid_attachments_policy("none"));
    }

    #[test]
    fn invalid_attachments_policies_rejected() {
        assert!(!is_valid_attachments_policy(""));
        assert!(!is_valid_attachments_policy("AUTO"));
        assert!(!is_valid_attachments_policy("Inline"));
        assert!(!is_valid_attachments_policy("always"));
        assert!(!is_valid_attachments_policy("never"));
        assert!(!is_valid_attachments_policy("detach"));
        assert!(!is_valid_attachments_policy(" auto"));
        assert!(!is_valid_attachments_policy("auto "));
    }

    // ── Response type serialization ──

    #[test]
    fn health_check_response_serializes() {
        let r = HealthCheckResponse {
            status: "ok".into(),
            health_level: "green".into(),
            environment: "development".into(),
            http_host: "0.0.0.0".into(),
            http_port: 8765,
            database_url: "sqlite:///data/test.db".into(),
            semantic_readiness: SemanticReadinessResponse {
                status: "ok".into(),
                detail: "aligned".into(),
            },
            pool_utilization: None,
            queues: None,
            disk: None,
            integrity: None,
            semantic_indexing: None,
            two_tier_indexing: None,
            recovery: None,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["semantic_readiness"]["status"], "ok");
        assert_eq!(json["http_port"], 8765);
    }

    #[test]
    fn project_response_serializes() {
        let r = ProjectResponse {
            id: 1,
            slug: "data-projects-test".into(),
            human_key: "/data/projects/test".into(),
            created_at: "2026-02-06T00:00:00Z".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["id"], 1);
        assert_eq!(json["slug"], "data-projects-test");
        assert_eq!(json["human_key"], "/data/projects/test");
    }

    #[test]
    fn agent_response_serializes_all_fields() {
        let r = AgentResponse {
            id: 42,
            name: "BlueLake".into(),
            program: "claude-code".into(),
            model: "opus-4.5".into(),
            task_description: "Testing".into(),
            inception_ts: "2026-02-06T00:00:00Z".into(),
            last_active_ts: "2026-02-06T01:00:00Z".into(),
            project_id: 1,
            attachments_policy: "auto".into(),
            reaper_exempt: false,
            capabilities: DEFAULT_AGENT_CAPABILITIES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            registration_token: None,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["name"], "BlueLake");
        assert_eq!(json["program"], "claude-code");
        assert_eq!(json["attachments_policy"], "auto");
        assert_eq!(json["id"], 42);
        assert_eq!(json["project_id"], 1);
        assert!(json["capabilities"].as_array().unwrap().len() >= 4);
    }

    #[test]
    fn agent_response_round_trips() {
        let original = AgentResponse {
            id: 42,
            name: "BlueLake".into(),
            program: "claude-code".into(),
            model: "opus-4.5".into(),
            task_description: "Testing".into(),
            inception_ts: "2026-02-06T00:00:00Z".into(),
            last_active_ts: "2026-02-06T01:00:00Z".into(),
            project_id: 1,
            attachments_policy: "auto".into(),
            reaper_exempt: false,
            capabilities: DEFAULT_AGENT_CAPABILITIES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            registration_token: None,
        };
        let json_str = serde_json::to_string(&original).unwrap();
        let deserialized: AgentResponse = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deserialized.name, original.name);
        assert_eq!(deserialized.id, original.id);
        assert_eq!(deserialized.program, original.program);
    }

    #[test]
    fn whois_response_flattens_agent_fields() {
        let r = WhoisResponse {
            agent: AgentResponse {
                id: 1,
                name: "RedFox".into(),
                program: "codex-cli".into(),
                model: "gpt-5".into(),
                task_description: String::new(),
                inception_ts: "2026-02-06T00:00:00Z".into(),
                last_active_ts: "2026-02-06T00:00:00Z".into(),
                project_id: 1,
                attachments_policy: "auto".into(),
                reaper_exempt: false,
                capabilities: DEFAULT_AGENT_CAPABILITIES
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                registration_token: None,
            },
            recent_commits: vec![CommitInfo {
                hexsha: "abc123".into(),
                summary: "test commit".into(),
                authored_ts: "2026-02-06T00:00:00Z".into(),
            }],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        // Agent fields are flattened into the top level
        assert_eq!(json["name"], "RedFox");
        assert_eq!(json["program"], "codex-cli");
        // Commits are nested
        assert_eq!(json["recent_commits"][0]["hexsha"], "abc123");
    }

    #[test]
    fn whois_response_empty_commits_array() {
        let r = WhoisResponse {
            agent: AgentResponse {
                id: 1,
                name: "BlueLake".into(),
                program: "claude-code".into(),
                model: "opus-4.5".into(),
                task_description: String::new(),
                inception_ts: String::new(),
                last_active_ts: String::new(),
                project_id: 1,
                attachments_policy: "none".into(),
                reaper_exempt: false,
                capabilities: DEFAULT_AGENT_CAPABILITIES
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                registration_token: None,
            },
            recent_commits: vec![],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!(json["recent_commits"].as_array().unwrap().is_empty());
    }

    // ── Path validation (ensure_project logic) ──

    #[test]
    fn absolute_paths_detected() {
        assert!(Path::new("/data/projects/test").is_absolute());
        assert!(Path::new("/").is_absolute());
        assert!(Path::new("/home/user/.config").is_absolute());
    }

    #[test]
    fn relative_paths_detected() {
        assert!(!Path::new("data/projects/test").is_absolute());
        assert!(!Path::new("./test").is_absolute());
        assert!(!Path::new("test").is_absolute());
        assert!(!Path::new("").is_absolute());
    }

    // ── Agent name validation (from core) ──

    #[test]
    fn valid_agent_names_accepted() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        assert!(is_valid_agent_name("BlueLake"));
        assert!(is_valid_agent_name("RedFox"));
        assert!(is_valid_agent_name("GoldHawk"));
    }

    #[test]
    fn invalid_agent_names_rejected() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        assert!(!is_valid_agent_name(""));
        assert!(!is_valid_agent_name("blue_lake")); // underscore not allowed
        assert!(!is_valid_agent_name("123"));
        assert!(!is_valid_agent_name("Blue Lake")); // space not allowed
        assert!(!is_valid_agent_name("EaglePeak")); // eagle is a noun, not adjective
        assert!(!is_valid_agent_name("BraveLion")); // brave not in adjective list
        assert!(!is_valid_agent_name("x")); // too short
    }

    // ── Whitespace trimming for program/model ──

    #[test]
    fn whitespace_only_program_is_empty_after_trim() {
        assert!("".trim().is_empty());
        assert!("  ".trim().is_empty());
        assert!("\t".trim().is_empty());
        assert!(!"claude-code".trim().is_empty());
    }

    #[test]
    fn whitespace_only_model_is_empty_after_trim() {
        assert!("".trim().is_empty());
        assert!("  ".trim().is_empty());
        assert!(!"opus-4.5".trim().is_empty());
    }

    // -----------------------------------------------------------------------
    // Tool validation rule tests (br-2841)
    // -----------------------------------------------------------------------

    // ── ensure_project validation ──

    #[test]
    fn ensure_project_rejects_relative_path() {
        // ensure_project requires absolute paths (starts with '/')
        let key = "relative/path/to/project";
        assert!(!Path::new(key).is_absolute());
    }

    #[test]
    fn ensure_project_rejects_empty_key() {
        assert!(!Path::new("").is_absolute());
    }

    #[test]
    fn ensure_project_accepts_root_path() {
        assert!(Path::new("/").is_absolute());
    }

    #[test]
    fn ensure_project_accepts_deeply_nested_path() {
        assert!(Path::new("/a/b/c/d/e/f/g").is_absolute());
    }

    #[test]
    fn path_has_ephemeral_root_detects_common_temp_locations() {
        use mcp_agent_mail_core::ephemeral::path_has_ephemeral_root;
        assert!(path_has_ephemeral_root(Path::new("/tmp/test-project")));
        assert!(path_has_ephemeral_root(Path::new("/var/tmp/test-project")));
        assert!(path_has_ephemeral_root(Path::new(
            "/private/tmp/test-project"
        )));
        assert!(path_has_ephemeral_root(Path::new(
            "/var/folders/aa/bb/T/test-project"
        )));
        assert!(path_has_ephemeral_root(Path::new("/dev/shm/test-session")));
        assert!(!path_has_ephemeral_root(Path::new(
            "/data/projects/not-temporary"
        )));
    }

    #[test]
    fn ephemeral_reroute_redirects_tmp_projects() {
        let mut config = Config::default();
        config.storage_root = mcp_agent_mail_core::config::default_storage_root_path();
        config.allow_ephemeral_projects_in_default_storage = false;
        config.ephemeral_mode = mcp_agent_mail_core::ephemeral::EphemeralMode::Auto;

        let rerouted = maybe_reroute_ephemeral_storage(&config, "/tmp/test-project");
        assert!(
            rerouted.is_some(),
            "tmp project should trigger auto-reroute"
        );
        let rerouted = rerouted.unwrap();
        assert_ne!(rerouted.storage_root, config.storage_root);
        assert!(
            rerouted
                .storage_root
                .to_string_lossy()
                .contains(".am-ephemeral"),
            "rerouted path should be under .am-ephemeral"
        );
    }

    #[test]
    fn ephemeral_reroute_skips_custom_storage_roots() {
        let config = Config {
            storage_root: PathBuf::from("/tmp/custom-storage-root"),
            allow_ephemeral_projects_in_default_storage: false,
            ..Config::default()
        };

        let rerouted = maybe_reroute_ephemeral_storage(&config, "/tmp/test-project");
        assert!(
            rerouted.is_none(),
            "custom storage root should skip auto-reroute"
        );
    }

    #[test]
    fn ephemeral_reroute_respects_deny_mode() {
        let mut config = Config::default();
        config.storage_root = mcp_agent_mail_core::config::default_storage_root_path();
        config.ephemeral_mode = mcp_agent_mail_core::ephemeral::EphemeralMode::Deny;

        // Deny mode treats all contexts as production, so no reroute
        let rerouted = maybe_reroute_ephemeral_storage(&config, "/tmp/test-project");
        assert!(rerouted.is_none(), "deny mode should skip auto-reroute");
    }

    #[test]
    fn ephemeral_reroute_deterministic_hash() {
        let mut config = Config::default();
        config.storage_root = mcp_agent_mail_core::config::default_storage_root_path();
        config.ephemeral_mode = mcp_agent_mail_core::ephemeral::EphemeralMode::Auto;

        let r1 = maybe_reroute_ephemeral_storage(&config, "/tmp/test-project");
        let r2 = maybe_reroute_ephemeral_storage(&config, "/tmp/test-project");
        assert_eq!(
            r1.as_ref().map(|c| &c.storage_root),
            r2.as_ref().map(|c| &c.storage_root),
            "same project path should produce the same isolated root"
        );

        let r3 = maybe_reroute_ephemeral_storage(&config, "/tmp/different-project");
        assert_ne!(
            r1.as_ref().map(|c| &c.storage_root),
            r3.as_ref().map(|c| &c.storage_root),
            "different project paths should produce different isolated roots"
        );
    }

    #[test]
    fn ephemeral_reroute_uses_custom_ephemeral_root() {
        let mut config = Config::default();
        config.storage_root = mcp_agent_mail_core::config::default_storage_root_path();
        config.ephemeral_mode = mcp_agent_mail_core::ephemeral::EphemeralMode::Auto;
        config.ephemeral_root = Some(PathBuf::from("/dev/shm/my-ephemeral"));

        let rerouted = maybe_reroute_ephemeral_storage(&config, "/tmp/test-project");
        assert!(rerouted.is_some());
        let root = rerouted.unwrap().storage_root;
        assert!(
            root.starts_with("/dev/shm/my-ephemeral"),
            "rerouted path should be under custom ephemeral root, got: {root:?}"
        );
    }

    #[test]
    fn ephemeral_reroute_skips_production_paths() {
        let mut config = Config::default();
        config.storage_root = mcp_agent_mail_core::config::default_storage_root_path();
        config.ephemeral_mode = mcp_agent_mail_core::ephemeral::EphemeralMode::Auto;

        let rerouted = maybe_reroute_ephemeral_storage(&config, "/data/projects/real-project");
        assert!(
            rerouted.is_none(),
            "production path should not trigger auto-reroute"
        );
    }

    // ── Agent name validation extended ──

    #[test]
    fn agent_name_validation_case_insensitive() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        // Validation is case-insensitive (lowercases before checking)
        assert!(is_valid_agent_name("BlueLake"));
        assert!(is_valid_agent_name("bluelake"));
        assert!(is_valid_agent_name("BLUELAKE"));
        assert!(is_valid_agent_name("bLuElAkE"));
    }

    #[test]
    fn agent_name_numbers_only_rejected() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        assert!(!is_valid_agent_name("12345"));
    }

    #[test]
    fn agent_name_special_chars_rejected() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        assert!(!is_valid_agent_name("Blue-Lake"));
        assert!(!is_valid_agent_name("Blue_Lake"));
        assert!(!is_valid_agent_name("Blue.Lake"));
        assert!(!is_valid_agent_name("Blue@Lake"));
    }

    #[test]
    fn agent_name_descriptive_names_rejected() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        // These look like agent names but use invalid adjectives/nouns
        assert!(!is_valid_agent_name("BackendHarmonizer"));
        assert!(!is_valid_agent_name("DatabaseMigrator"));
        assert!(!is_valid_agent_name("UIRefactorer"));
    }

    // ── Attachments policy validation extended ──

    #[test]
    fn attachments_policy_all_valid_values() {
        for policy in &["auto", "inline", "file", "none"] {
            assert!(
                is_valid_attachments_policy(policy),
                "Policy '{policy}' should be valid"
            );
        }
    }

    #[test]
    fn attachments_policy_boundary_values() {
        // Near-misses and common mistakes
        assert!(!is_valid_attachments_policy("auto\n"));
        assert!(!is_valid_attachments_policy("\nauto"));
        assert!(!is_valid_attachments_policy("auto\0"));
        assert!(!is_valid_attachments_policy("inlined"));
        assert!(!is_valid_attachments_policy("files"));
    }

    // ── us_to_ms_ceil correctness ──

    #[test]
    fn us_to_ms_ceil_rounds_up() {
        assert_eq!(us_to_ms_ceil(0), 0);
        assert_eq!(us_to_ms_ceil(1), 1); // 1µs → 1ms (rounded up)
        assert_eq!(us_to_ms_ceil(999), 1); // 999µs → 1ms
        assert_eq!(us_to_ms_ceil(1000), 1); // exactly 1ms
        assert_eq!(us_to_ms_ceil(1001), 2); // 1001µs → 2ms
        assert_eq!(us_to_ms_ceil(1500), 2); // 1.5ms → 2ms
        assert_eq!(us_to_ms_ceil(2000), 2); // exactly 2ms
    }

    #[test]
    fn us_to_ms_ceil_handles_max() {
        // Should not overflow/panic with u64::MAX
        let result = us_to_ms_ceil(u64::MAX);
        // u64::MAX.saturating_add(999) → u64::MAX; u64::MAX / 1000 = 18446744073709551
        assert!(result > 0);
    }

    // ── Response serialization — optional fields omitted ──

    #[test]
    fn health_check_omits_optional_null_fields() {
        let r = HealthCheckResponse {
            status: "ok".into(),
            health_level: "green".into(),
            environment: "test".into(),
            http_host: "localhost".into(),
            http_port: 8765,
            database_url: "sqlite:///:memory:".into(),
            semantic_readiness: SemanticReadinessResponse {
                status: "ok".into(),
                detail: "memory".into(),
            },
            pool_utilization: None,
            queues: None,
            disk: None,
            integrity: None,
            semantic_indexing: None,
            two_tier_indexing: None,
            recovery: None,
        };
        let json_str = serde_json::to_string(&r).unwrap();
        assert!(!json_str.contains("pool_utilization"));
        assert!(!json_str.contains("queues"));
        assert!(!json_str.contains("disk"));
        assert!(!json_str.contains("integrity"));
        assert!(!json_str.contains("semantic_indexing"));
        assert!(!json_str.contains("two_tier_indexing"));
        assert!(!json_str.contains("recovery"));
    }

    #[test]
    fn health_check_reports_error_when_archive_is_ahead_of_sqlite_index() {
        let _guard = HEALTH_CHECK_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("stale-health-check.sqlite3");
        let project_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("04");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::create_dir_all(&messages_dir).expect("create messages dir");
        std::fs::write(agent_dir.join("profile.json"), "{}").expect("write agent profile");
        std::fs::write(
            messages_dir.join("2026-04-01T12-00-00Z__hello__7.md"),
            r#"---json
{
  "id": 7,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "Hello",
  "importance": "normal",
  "created_ts": "2026-04-01T12:00:00Z"
}
---

body
"#,
        )
        .expect("write canonical message");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        drop(conn);

        with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", &format!("sqlite:///{}", db_path.display())),
                ("STORAGE_ROOT", &storage_root.display().to_string()),
            ],
            || {
                Config::reset_cached();
                let ctx = McpContext::new(Cx::for_testing(), 1);
                let response = health_check(&ctx).expect("health_check should serialize");
                let value: serde_json::Value =
                    serde_json::from_str(&response).expect("parse health_check json");
                assert_eq!(value["status"], "error");
                assert_eq!(value["semantic_readiness"]["status"], "fail");
                assert!(
                    value["semantic_readiness"]["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("archive inventory is ahead")),
                    "health_check should surface archive/db drift details: {value}"
                );
            },
        );
    }

    #[test]
    fn health_check_reports_error_when_project_identity_differs_with_equal_counts() {
        let _guard = HEALTH_CHECK_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp
            .path()
            .join("stale-project-identity-health-check.sqlite3");
        let project_dir = storage_root.join("projects").join("archive-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"archive-project","human_key":"/archive-project"}"#,
        )
        .expect("write project metadata");
        std::fs::write(agent_dir.join("profile.json"), "{}").expect("write agent profile");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?1, ?2, ?3, ?4)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("wrong-project".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("/wrong-project".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
            ],
        )
        .expect("insert wrong project");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("Alice".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("coder".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("test".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text(String::new()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("auto".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("auto".to_string()),
            ],
        )
        .expect("insert agent");
        drop(conn);

        with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", &format!("sqlite:///{}", db_path.display())),
                ("STORAGE_ROOT", &storage_root.display().to_string()),
            ],
            || {
                Config::reset_cached();
                let ctx = McpContext::new(Cx::for_testing(), 1);
                let response = health_check(&ctx).expect("health_check should serialize");
                let value: serde_json::Value =
                    serde_json::from_str(&response).expect("parse health_check json");
                assert_eq!(value["status"], "error");
                assert_eq!(value["semantic_readiness"]["status"], "fail");
                assert!(
                    value["semantic_readiness"]["detail"]
                        .as_str()
                        .is_some_and(|detail| {
                            detail.contains(
                                "missing archive project(s) in db: archive-project (/archive-project)"
                            )
                        }),
                    "health_check should surface missing archive project identity: {value}"
                );
            },
        );
    }

    #[test]
    fn health_check_does_not_initialize_missing_sqlite() {
        let _guard = HEALTH_CHECK_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("create storage root");
        let db_path = temp.path().join("missing-health-check.sqlite3");

        with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", &format!("sqlite:///{}", db_path.display())),
                ("STORAGE_ROOT", &storage_root.display().to_string()),
            ],
            || {
                Config::reset_cached();
                let ctx = McpContext::new(Cx::for_testing(), 1);
                let response = health_check(&ctx).expect("health_check should serialize");
                let value: serde_json::Value =
                    serde_json::from_str(&response).expect("parse health_check json");
                assert_eq!(value["status"], "error");
                assert_eq!(value["semantic_readiness"]["status"], "fail");
                assert!(
                    value["semantic_readiness"]["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("refuses to initialize")),
                    "missing sqlite detail should explain non-mutating refusal: {value}"
                );
            },
        );

        assert!(
            !db_path.exists(),
            "health_check must not create a missing sqlite file"
        );
    }

    #[test]
    fn list_agents_uses_archive_snapshot_when_live_db_is_stale() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("stale-list-agents.sqlite3");
        let project_dir = storage_root.join("projects").join("archive-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"archive-project","human_key":"/archive-project"}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-04-01T12:00:00Z","last_active_ts":"2026-04-01T12:00:00Z"}"#,
        )
        .expect("write agent profile");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        drop(conn);

        with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", &format!("sqlite:///{}", db_path.display())),
                ("STORAGE_ROOT", &storage_root.display().to_string()),
            ],
            || {
                Config::reset_cached();
                let rt = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime");
                rt.block_on(async {
                    let cx = Cx::for_testing();
                    let ctx = McpContext::new(cx.clone(), 1);
                    let response = list_agents(&ctx, "/archive-project".to_string())
                        .await
                        .expect("list_agents should succeed");
                    let value: serde_json::Value =
                        serde_json::from_str(&response).expect("parse list_agents json");
                    let agents = value.as_array().expect("agents array");
                    assert_eq!(agents.len(), 1);
                    assert_eq!(agents[0]["name"], "Alice");
                });
            },
        );
    }

    #[test]
    fn redact_database_url_memory_db() {
        // In-memory SQLite should pass through unchanged
        assert_eq!(
            redact_database_url("sqlite:///:memory:"),
            "sqlite:///:memory:"
        );
    }

    #[test]
    fn redact_database_url_multiple_at_signs() {
        // Edge case: multiple @ signs — last one is the host separator
        let result = redact_database_url("postgres://user:p@ss@host/db");
        assert_eq!(result, "postgres://****@host/db");
    }

    #[test]
    fn resolve_identity_from_project_keys_falls_back_to_human_key() {
        let raw_project_key = "test-project".to_string();
        let human_key = format!("/tmp/test-pane-identity-human-key-{}", std::process::id());
        let pane = "%17";
        let written_path =
            mcp_agent_mail_core::write_identity(&human_key, pane, "BlueLake").expect("write");

        let resolved = resolve_identity_from_project_keys(&[raw_project_key, human_key], pane)
            .expect("resolve identity across project keys");
        assert_eq!(resolved.0, "BlueLake");
        assert_eq!(resolved.1, written_path);

        let _ = std::fs::remove_file(&written_path);
        if let Some(parent) = written_path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}
