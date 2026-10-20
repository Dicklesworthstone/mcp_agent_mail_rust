//! Build slot cluster tools (coarse concurrency control)
//!
//! Ported from legacy Python:
//! - Only meaningful when `WORKTREES_ENABLED=1`
//! - Stores per-slot leases as JSON files under the per-project archive root:
//!   `{storage_root}/projects/{project_slug}/build_slots/{slot}/{agent_hash}.json`
//! - Conflicts are detected by scanning active (non-expired) leases.
//! - A per-slot filesystem lock serializes scan/resolve/write transactions.
//!   Contention is retryable; unreadable lease state never means an empty slot.
//! - Retired/deregistered identities cannot acquire or renew a lease. An
//!   authorized inactive holder may still release its existing lease.
//! - Renew/release update the files actually inspected, including all owned
//!   active aliases left by older branch-keyed leases. Filenames are not owners.

use fastmcp::prelude::*;
use mcp_agent_mail_core::Config;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tool_util::{db_outcome_to_mcp_result, get_db_pool, legacy_tool_error, resolve_project};

static LEASE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildSlotLease {
    pub slot: String,
    pub agent: String,
    pub branch: Option<String>,
    pub exclusive: bool,
    pub acquired_ts: String,
    pub expires_ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_ts: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquireBuildSlotResponse {
    pub granted: BuildSlotLease,
    pub conflicts: Vec<BuildSlotLease>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewBuildSlotResponse {
    pub renewed: bool,
    pub expires_ts: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseBuildSlotResponse {
    pub released: bool,
    pub released_at: String,
}

fn safe_component(value: &str) -> String {
    let safe = value
        .trim()
        .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|', ' '], "_");
    // Prevent path traversal via special components.
    if safe.is_empty() || safe == "." || safe == ".." {
        "unknown".to_string()
    } else {
        safe
    }
}

fn project_archive_root(config: &Config, project_slug: &str) -> PathBuf {
    config.storage_root.join("projects").join(project_slug)
}

fn slot_dir(project_root: &Path, slot: &str) -> PathBuf {
    project_root.join("build_slots").join(safe_component(slot))
}

fn holder_identity(agent_name: &str, branch: Option<&str>) -> String {
    branch.map_or_else(|| agent_name.to_string(), |b| format!("{agent_name}:::{b}"))
}

fn lease_path_for_holder(slot_path: &Path, agent_name: &str, _branch: Option<&str>) -> PathBuf {
    // The lease file is keyed by agent alone so the same agent reuses a
    // single file across branch changes (see the one-agent-one-lease
    // invariant enforced by `existing_agent_lease_path` and
    // `collect_slot_conflicts`). The sha256 prefix keeps the on-disk name
    // stable even when `safe_component` sanitizes the agent name.
    let digest = Sha256::digest(agent_name.as_bytes());
    let short_hash = &hex::encode(digest)[..16];
    let holder_id = format!("{}--{short_hash}", safe_component(agent_name));
    slot_path.join(format!("{holder_id}.json"))
}

fn lease_matches_holder(lease: &BuildSlotLease, agent_name: &str, _branch: Option<&str>) -> bool {
    // Branches can change after acquisition. Older branch-keyed files can
    // also coexist, so renew/release must handle every matching stored lease
    // rather than infer ownership or the current path from a branch name.
    lease.agent == agent_name
}

fn legacy_lease_path_for_holder(
    slot_path: &Path,
    agent_name: &str,
    branch: Option<&str>,
) -> PathBuf {
    let holder_id = safe_component(&holder_identity(agent_name, branch));
    slot_path.join(format!("{holder_id}.json"))
}

fn existing_agent_lease_path(
    slot_path: &Path,
    agent_name: &str,
    branch: Option<&str>,
) -> Option<PathBuf> {
    let mut same_agent_paths = Vec::new();
    let entries = std::fs::read_dir(slot_path).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(lease) = serde_json::from_str::<BuildSlotLease>(&text) else {
            continue;
        };
        if lease.agent != agent_name {
            continue;
        }
        if lease.branch.as_deref() == branch {
            return Some(path);
        }
        same_agent_paths.push(path);
    }
    if same_agent_paths.len() == 1 {
        same_agent_paths.pop()
    } else {
        None
    }
}

fn resolve_holder_lease_path(slot_path: &Path, agent_name: &str, branch: Option<&str>) -> PathBuf {
    let preferred = lease_path_for_holder(slot_path, agent_name, branch);
    if preferred.exists() {
        return preferred;
    }

    let legacy = legacy_lease_path_for_holder(slot_path, agent_name, branch);
    if legacy.exists() {
        return legacy;
    }

    if let Some(existing) = existing_agent_lease_path(slot_path, agent_name, branch) {
        return existing;
    }

    preferred
}

fn compute_branch(repo_path: &str) -> Option<String> {
    let repo = git2::Repository::discover(repo_path).ok()?;
    let head = repo.head().ok()?;
    head.shorthand().ok().map(str::to_string)
}

fn read_active_leases(
    slot_path: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> std::io::Result<Vec<BuildSlotLease>> {
    Ok(read_active_lease_entries(slot_path, now)?
        .into_iter()
        .map(|entry| entry.lease)
        .collect())
}

#[derive(Debug)]
struct ActiveSlotLease {
    path: PathBuf,
    lease: BuildSlotLease,
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// Retain the exact path alongside the validated record. A second filename
/// resolution can choose a stale preferred file instead of this active lease,
/// or even a different holder's file. Callers keep the slot lock throughout.
fn read_active_lease_entries(
    slot_path: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> std::io::Result<Vec<ActiveSlotLease>> {
    let mut results = Vec::new();
    let entries = match std::fs::read_dir(slot_path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(results),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|error| {
            std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        let lease: BuildSlotLease = serde_json::from_str(&text).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid build slot lease {}: {error}", path.display()),
            )
        })?;
        let exp = chrono::DateTime::parse_from_rfc3339(&lease.expires_ts).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid build slot expiry {}: {error}", path.display()),
            )
        })?;
        let expires_at = exp.with_timezone(&chrono::Utc);
        if expires_at <= now {
            // Lease expired. We do not delete the file here to prevent a TOCTOU race
            // where another agent just renewed it. The file will be overwritten on next acquire.
            continue;
        }
        results.push(ActiveSlotLease {
            path,
            lease,
            expires_at,
        });
    }
    results.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(results)
}

/// Hold one stable lock inode throughout a slot's read/modify/write operation.
/// Never unlink the lock file: another process may already hold an open handle
/// to it. File close releases the OS lock, including on errors and unwinding.
/// `None` means the slot directory is absent and no creation was requested.
fn try_lock_slot(slot_path: &Path, create: bool) -> std::io::Result<Option<std::fs::File>> {
    if create {
        std::fs::create_dir_all(slot_path)?;
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(slot_path.join(".slot.lock"))
    {
        Ok(file) => file,
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    // Non-blocking: never park an async executor thread behind another process.
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(Some(file))
}

fn slot_io_error(error: std::io::Error) -> McpError {
    let contended = error.kind() == std::io::ErrorKind::WouldBlock
        || error
            .raw_os_error()
            .is_some_and(|code| Some(code) == fs2::lock_contended_error().raw_os_error());
    if contended {
        return legacy_tool_error(
            "BUILD_SLOT_BUSY",
            "Another build-slot operation is in progress. No lease was changed; retry this call.",
            true,
            serde_json::json!({ "retry_after_ms": 50 }),
        );
    }
    if error.kind() == std::io::ErrorKind::InvalidData {
        return legacy_tool_error(
            "BUILD_SLOT_STATE_INVALID",
            format!(
                "Cannot safely read build-slot state: {error}. Repair the affected lease file before retrying."
            ),
            false,
            serde_json::json!({ "operation": "read_build_slot_leases" }),
        );
    }
    McpError::internal_error(format!("build slot I/O failed: {error}"))
}

fn unique_tmp_lease_path(path: &Path) -> PathBuf {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let pid = std::process::id();
    let seq = LEASE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!(".{file_name}.{pid}.{nanos}.{seq}.tmp"))
}

fn write_lease_json(path: &Path, lease: &BuildSlotLease) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text =
        serde_json::to_string_pretty(lease).map_err(|e| std::io::Error::other(e.to_string()))?;

    // Write atomically to prevent race conditions during read_active_leases
    let tmp_path = unique_tmp_lease_path(path);
    std::fs::write(&tmp_path, text)?;
    match std::fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

fn compute_renewed_expiry(
    now: chrono::DateTime<chrono::Utc>,
    current_expires_ts: &str,
    extend_seconds: i64,
) -> String {
    let current_expiry = chrono::DateTime::parse_from_rfc3339(current_expires_ts)
        .map_or(now, |value| value.with_timezone(&chrono::Utc));
    (std::cmp::max(now, current_expiry) + chrono::Duration::seconds(extend_seconds)).to_rfc3339()
}

#[derive(Debug, Clone, Copy)]
enum SlotOperation {
    Acquire,
    Renew,
    Release,
}

fn enforce_slot_lifecycle(
    agent: &mcp_agent_mail_db::AgentRow,
    operation: SlotOperation,
) -> McpResult<()> {
    // Withdrawing existing work must remain possible after retirement. This
    // function does not grant actor authority; the session check runs first.
    if matches!(operation, SlotOperation::Release) {
        return Ok(());
    }
    let (code, state) =
        if mcp_agent_mail_db::models::task_description_uses_reserved_deregistered_prefix(
            &agent.task_description,
        ) {
            // Imported Python tombstones need not also carry retired_at.
            ("AGENT_DEREGISTERED", "deregistered")
        } else if agent.retired_at.is_some() {
            ("AGENT_RETIRED", "retired")
        } else {
            return Ok(());
        };
    let action = match operation {
        SlotOperation::Acquire => "acquire",
        SlotOperation::Renew => "renew",
        SlotOperation::Release => "release",
    };
    Err(legacy_tool_error(
        code,
        format!(
            "Agent '{}' is {state} and cannot {action} a build-slot lease. \
             Use an active identity for new work; existing leases may still be released.",
            agent.name,
        ),
        false,
        serde_json::json!({
            "agent": agent.name,
            "project_id": agent.project_id,
            "operation": action,
            "retired_at": agent.retired_at,
        }),
    ))
}

async fn resolve_authorized_agent_name(
    ctx: &McpContext,
    pool: &mcp_agent_mail_db::pool::DbPool,
    project_id: i64,
    agent_name: &str,
    operation: SlotOperation,
) -> McpResult<String> {
    let agent_name = mcp_agent_mail_core::models::normalize_agent_name(agent_name)
        .unwrap_or_else(|| agent_name.to_string());
    let agent = match mcp_agent_mail_db::queries::get_agent(ctx.cx(), pool, project_id, &agent_name)
        .await
    {
        asupersync::Outcome::Ok(agent) => agent,
        asupersync::Outcome::Err(mcp_agent_mail_db::DbError::NotFound { .. }) => {
            // Trusted-local callers historically need not register before
            // taking a slot. An unknown name still cannot borrow the identity
            // of a session already bound in this project.
            mcp_agent_mail_db::AgentRow {
                project_id,
                name: agent_name,
                ..mcp_agent_mail_db::AgentRow::default()
            }
        }
        other => db_outcome_to_mcp_result(other)?,
    };
    crate::session_identity::authorize_actor(ctx, &agent, false, "manage build slots")?;
    enforce_slot_lifecycle(&agent, operation)?;
    Ok(agent.name)
}

fn collect_slot_conflicts(
    active: Vec<BuildSlotLease>,
    agent_name: &str,
    _branch: Option<&str>,
    request_exclusive: bool,
) -> Vec<BuildSlotLease> {
    let mut conflicts = Vec::new();
    for entry in active {
        if entry.released_ts.is_some() {
            continue;
        }
        // Renewing/reacquiring your own lease should not self-conflict.
        if entry.agent == agent_name {
            continue;
        }
        // Exclusive request conflicts with any active holder.
        // Shared request conflicts only with existing exclusive holders.
        if request_exclusive || entry.exclusive {
            conflicts.push(entry);
        }
    }
    conflicts
}

// All filesystem transactions below are synchronous and contain no await.
// Their lock remains live through conflict detection, path resolution and the
// atomic lease write. Advisory conflicts still return a granted lease, as before.
fn acquire_slot_lease(
    slot_path: &Path,
    slot: &str,
    agent_name: &str,
    branch: Option<String>,
    ttl: i64,
    is_exclusive: bool,
) -> std::io::Result<AcquireBuildSlotResponse> {
    let _slot_lock = try_lock_slot(slot_path, true)?;
    let now = chrono::Utc::now();
    let active = read_active_leases(slot_path, now)?;
    let conflicts = collect_slot_conflicts(active, agent_name, branch.as_deref(), is_exclusive);
    let lease_path = resolve_holder_lease_path(slot_path, agent_name, branch.as_deref());
    let granted = BuildSlotLease {
        slot: slot.to_string(),
        agent: agent_name.to_string(),
        branch,
        exclusive: is_exclusive,
        acquired_ts: now.to_rfc3339(),
        expires_ts: (now + chrono::Duration::seconds(ttl)).to_rfc3339(),
        released_ts: None,
    };
    write_lease_json(&lease_path, &granted)?;
    Ok(AcquireBuildSlotResponse { granted, conflicts })
}

fn renew_slot_lease(
    slot_path: &Path,
    agent_name: &str,
    branch: Option<&str>,
    extend: i64,
) -> std::io::Result<RenewBuildSlotResponse> {
    let missing = RenewBuildSlotResponse {
        renewed: false,
        expires_ts: String::new(),
    };
    let Some(_slot_lock) = try_lock_slot(slot_path, false)? else {
        return Ok(missing);
    };
    let now = chrono::Utc::now();
    let owned: Vec<_> = read_active_lease_entries(slot_path, now)?
        .into_iter()
        .filter(|entry| {
            lease_matches_holder(&entry.lease, agent_name, branch)
                && entry.lease.released_ts.is_none()
        })
        .collect();
    let Some(latest) = owned.iter().map(|entry| entry.expires_at).max() else {
        return Ok(missing);
    };
    // One response expiry must describe every active alias. Extend from the
    // latest existing expiry, never shorten a longer-lived owned lease.
    let new_exp = compute_renewed_expiry(now, &latest.to_rfc3339(), extend);
    for mut entry in owned {
        entry.lease.expires_ts.clone_from(&new_exp);
        write_lease_json(&entry.path, &entry.lease)?;
    }
    Ok(RenewBuildSlotResponse {
        renewed: true,
        expires_ts: new_exp,
    })
}

fn release_slot_lease(
    slot_path: &Path,
    agent_name: &str,
    branch: Option<&str>,
) -> std::io::Result<ReleaseBuildSlotResponse> {
    let missing = ReleaseBuildSlotResponse {
        released: false,
        released_at: String::new(),
    };
    let Some(_slot_lock) = try_lock_slot(slot_path, false)? else {
        return Ok(missing);
    };
    let now = chrono::Utc::now();
    let active = read_active_lease_entries(slot_path, now)?;
    let released_now = now.to_rfc3339();
    let mut response = missing;
    for mut entry in active {
        if !lease_matches_holder(&entry.lease, agent_name, branch) {
            continue;
        }
        let released_at = match &entry.lease.released_ts {
            Some(existing) => existing.clone(),
            None => {
                entry.lease.released_ts = Some(released_now.clone());
                write_lease_json(&entry.path, &entry.lease)?;
                released_now.clone()
            }
        };
        // A previously released alias must not short-circuit a remaining
        // active one after an interrupted release. Only successful completion
        // of the whole inventory reports success; an I/O error can leave a
        // released prefix for the next retry, never an invented rollback.
        response.released = true;
        // Choose a deterministic recorded marker without rewriting old ones.
        // Repeating a completed release returns the same marker, not "now".
        if released_at > response.released_at {
            response.released_at = released_at;
        }
    }
    Ok(response)
}

fn worktrees_required() -> McpError {
    legacy_tool_error(
        "FEATURE_DISABLED",
        "Build slots are disabled. Enable WORKTREES_ENABLED to use this tool.",
        true,
        serde_json::json!({ "feature": "worktrees", "env_var": "WORKTREES_ENABLED" }),
    )
}

/// Acquire a build slot (advisory), optionally exclusive. Returns conflicts when another holder is active.
///
/// # Conformance
/// Python-parity.
#[tool(
    description = "Acquire a build slot (advisory), optionally exclusive. Returns conflicts when another holder is active."
)]
pub async fn acquire_build_slot(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    slot: String,
    ttl_seconds: Option<i64>,
    exclusive: Option<bool>,
) -> McpResult<String> {
    let config = &Config::get();
    if !config.worktrees_enabled {
        return Err(worktrees_required());
    }

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);
    let agent_name =
        resolve_authorized_agent_name(ctx, &pool, project_id, &agent_name, SlotOperation::Acquire)
            .await?;
    let ttl = ttl_seconds.map_or(3600, |t| t.clamp(60, 31_536_000)); // 1 hour default
    let branch = compute_branch(&project.human_key);
    let project_root = project_archive_root(config, &project.slug);
    let slot_path = slot_dir(&project_root, &slot);
    let response = acquire_slot_lease(
        &slot_path,
        &slot,
        &agent_name,
        branch,
        ttl,
        exclusive.unwrap_or(true),
    )
    .map_err(slot_io_error)?;
    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Extend expiry for an existing build slot lease. No-op if missing.
///
/// # Conformance
/// Python-parity.
#[tool(description = "Extend expiry for an existing build slot lease. No-op if missing.")]
pub async fn renew_build_slot(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    slot: String,
    extend_seconds: Option<i64>,
) -> McpResult<String> {
    let config = &Config::get();
    if !config.worktrees_enabled {
        return Err(worktrees_required());
    }

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);
    let agent_name =
        resolve_authorized_agent_name(ctx, &pool, project_id, &agent_name, SlotOperation::Renew)
            .await?;
    let extend = extend_seconds.map_or(1800, |t| t.clamp(60, 31_536_000)); // 30 minutes default
    let branch = compute_branch(&project.human_key);
    let project_root = project_archive_root(config, &project.slug);
    let slot_path = slot_dir(&project_root, &slot);
    let response = renew_slot_lease(&slot_path, &agent_name, branch.as_deref(), extend)
        .map_err(slot_io_error)?;
    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Mark an active slot lease as released (non-destructive; keeps JSON with `released_ts`).
///
/// # Conformance
/// Python-parity.
#[tool(
    description = "Mark an active slot lease as released (non-destructive; keeps JSON with released_ts)."
)]
pub async fn release_build_slot(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    slot: String,
) -> McpResult<String> {
    let config = &Config::get();
    if !config.worktrees_enabled {
        return Err(worktrees_required());
    }

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);
    let agent_name =
        resolve_authorized_agent_name(ctx, &pool, project_id, &agent_name, SlotOperation::Release)
            .await?;
    let branch = compute_branch(&project.human_key);
    let project_root = project_archive_root(config, &project.slug);
    let slot_path = slot_dir(&project_root, &slot);
    let response =
        release_slot_lease(&slot_path, &agent_name, branch.as_deref()).map_err(slot_io_error)?;
    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // safe_component
    // -----------------------------------------------------------------------

    #[test]
    fn safe_component_simple() {
        assert_eq!(safe_component("myslot"), "myslot");
    }

    #[test]
    fn safe_component_replaces_slashes() {
        assert_eq!(safe_component("path/to/slot"), "path_to_slot");
    }

    #[test]
    fn safe_component_replaces_backslash() {
        assert_eq!(safe_component("path\\to\\slot"), "path_to_slot");
    }

    #[test]
    fn safe_component_replaces_colon() {
        assert_eq!(safe_component("C:slot"), "C_slot");
    }

    #[test]
    fn safe_component_replaces_spaces() {
        assert_eq!(safe_component("my slot"), "my_slot");
    }

    #[test]
    fn safe_component_replaces_angle_brackets() {
        assert_eq!(safe_component("<slot>"), "_slot_");
    }

    #[test]
    fn safe_component_replaces_pipe() {
        assert_eq!(safe_component("a|b"), "a_b");
    }

    #[test]
    fn safe_component_replaces_star_and_question() {
        assert_eq!(safe_component("glob*pattern?"), "glob_pattern_");
    }

    #[test]
    fn safe_component_replaces_double_quote() {
        assert_eq!(safe_component(r#"a"b"#), "a_b");
    }

    #[test]
    fn safe_component_trims_whitespace() {
        assert_eq!(safe_component("  hello  "), "hello");
    }

    #[test]
    fn safe_component_dot_is_rejected() {
        assert_eq!(safe_component("."), "unknown");
    }

    #[test]
    fn safe_component_dotdot_is_rejected() {
        assert_eq!(safe_component(".."), "unknown");
    }

    #[test]
    fn safe_component_empty_returns_unknown() {
        assert_eq!(safe_component(""), "unknown");
    }

    #[test]
    fn safe_component_only_whitespace_returns_unknown() {
        assert_eq!(safe_component("   "), "unknown");
    }

    #[test]
    fn safe_component_multiple_special_chars() {
        assert_eq!(safe_component("a/b\\c:d*e"), "a_b_c_d_e");
    }

    // -----------------------------------------------------------------------
    // project_archive_root
    // -----------------------------------------------------------------------

    #[test]
    fn project_archive_root_builds_path() {
        let config = Config {
            storage_root: PathBuf::from("/data/archives"),
            ..Config::default()
        };
        let root = project_archive_root(&config, "my-project");
        assert_eq!(root, PathBuf::from("/data/archives/projects/my-project"));
    }

    // -----------------------------------------------------------------------
    // slot_dir
    // -----------------------------------------------------------------------

    #[test]
    fn slot_dir_sanitizes_name() {
        let root = PathBuf::from("/archive/my-project");
        let dir = slot_dir(&root, "my slot/name");
        assert_eq!(
            dir,
            PathBuf::from("/archive/my-project/build_slots/my_slot_name")
        );
    }

    #[test]
    fn lease_path_for_holder_is_stable_for_the_same_agent() {
        let slot_path = PathBuf::from("/archive/my-project/build_slots/default");
        let slash_branch = lease_path_for_holder(&slot_path, "BlueLake", Some("feature/x"));
        let underscore_branch = lease_path_for_holder(&slot_path, "BlueLake", Some("feature_x"));
        assert_eq!(
            slash_branch, underscore_branch,
            "the same agent should reuse a single lease file across branch changes"
        );
        let file_name = slash_branch
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap();
        assert!(file_name.starts_with("BlueLake--"));
        assert!(
            Path::new(file_name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        );
    }

    #[test]
    fn resolve_holder_lease_path_prefers_existing_legacy_file() {
        let slot_dir = tempfile::tempdir().unwrap();
        let legacy_path =
            legacy_lease_path_for_holder(slot_dir.path(), "BlueLake", Some("feature/x"));
        std::fs::write(&legacy_path, "{}").unwrap();

        let resolved = resolve_holder_lease_path(slot_dir.path(), "BlueLake", Some("feature/x"));
        assert_eq!(resolved, legacy_path);
    }

    #[test]
    fn resolve_holder_lease_path_reuses_existing_same_agent_file_after_branch_change() {
        let slot_dir = tempfile::tempdir().unwrap();
        let existing_path = slot_dir.path().join("BlueLake--current.json");
        let lease = make_lease("BlueLake", Some("main"), true);
        std::fs::write(&existing_path, serde_json::to_string(&lease).unwrap()).unwrap();

        let resolved = resolve_holder_lease_path(slot_dir.path(), "BlueLake", Some("feature/x"));
        assert_eq!(resolved, existing_path);
    }

    #[test]
    fn lease_matches_holder_ignores_branch_changes_for_same_agent() {
        let lease = make_lease("BlueLake", Some("main"), true);
        assert!(lease_matches_holder(&lease, "BlueLake", Some("main")));
        assert!(lease_matches_holder(&lease, "BlueLake", Some("feature")));
    }

    #[test]
    fn compute_renewed_expiry_extends_from_existing_future_expiry() {
        let now = chrono::Utc::now();
        let existing_expiry = (now + chrono::Duration::minutes(30)).to_rfc3339();
        let renewed = compute_renewed_expiry(now, &existing_expiry, 600);
        let renewed_ts = chrono::DateTime::parse_from_rfc3339(&renewed)
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(
            renewed_ts >= now + chrono::Duration::minutes(40),
            "renewing an active lease must not shorten it"
        );
    }

    #[test]
    fn read_active_leases_rejects_invalid_expiration() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();

        let valid = BuildSlotLease {
            slot: "slot-a".to_string(),
            agent: "agent-valid".to_string(),
            branch: Some("main".to_string()),
            exclusive: true,
            acquired_ts: now.to_rfc3339(),
            expires_ts: (now + chrono::Duration::hours(1)).to_rfc3339(),
            released_ts: None,
        };
        std::fs::write(
            dir.path().join("valid.json"),
            serde_json::to_string(&valid).unwrap(),
        )
        .unwrap();

        let invalid = serde_json::json!({
            "slot": "slot-a",
            "agent": "agent-invalid",
            "branch": "main",
            "exclusive": true,
            "acquired_ts": now.to_rfc3339(),
            "expires_ts": "not-a-timestamp",
            "released_ts": null
        });
        std::fs::write(dir.path().join("invalid.json"), invalid.to_string()).unwrap();

        let error = read_active_leases(dir.path(), now).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("invalid.json"));
    }

    // -----------------------------------------------------------------------
    // worktrees_required error (br-3h13.4.5)
    // -----------------------------------------------------------------------

    #[test]
    fn worktrees_required_error_contains_feature_disabled() {
        let err = worktrees_required();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("FEATURE_DISABLED") || msg.contains("disabled"),
            "error should mention FEATURE_DISABLED: {msg}"
        );
    }

    #[test]
    fn worktrees_required_error_mentions_env_var() {
        let err = worktrees_required();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("WORKTREES_ENABLED"),
            "error should mention WORKTREES_ENABLED env var: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // BuildSlotLease serde (br-3h13.4.5)
    // -----------------------------------------------------------------------

    #[test]
    fn lease_round_trip_serde() {
        let now = chrono::Utc::now();
        let lease = BuildSlotLease {
            slot: "build-1".to_string(),
            agent: "GoldFox".to_string(),
            branch: Some("main".to_string()),
            exclusive: true,
            acquired_ts: now.to_rfc3339(),
            expires_ts: (now + chrono::Duration::hours(1)).to_rfc3339(),
            released_ts: None,
        };
        let json = serde_json::to_string(&lease).unwrap();
        let parsed: BuildSlotLease = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.slot, "build-1");
        assert_eq!(parsed.agent, "GoldFox");
        assert_eq!(parsed.branch, Some("main".to_string()));
        assert!(parsed.exclusive);
        assert!(parsed.released_ts.is_none());
    }

    #[test]
    fn lease_with_released_ts() {
        let now = chrono::Utc::now();
        let lease = BuildSlotLease {
            slot: "build-2".to_string(),
            agent: "SilverWolf".to_string(),
            branch: None,
            exclusive: false,
            acquired_ts: now.to_rfc3339(),
            expires_ts: now.to_rfc3339(),
            released_ts: Some(now.to_rfc3339()),
        };
        let json = serde_json::to_string(&lease).unwrap();
        let parsed: BuildSlotLease = serde_json::from_str(&json).unwrap();
        assert!(parsed.released_ts.is_some());
        assert!(!parsed.exclusive);
        assert!(parsed.branch.is_none());
    }

    // -----------------------------------------------------------------------
    // Response serde (br-3h13.4.5)
    // -----------------------------------------------------------------------

    #[test]
    fn acquire_response_serde() {
        let now = chrono::Utc::now();
        let granted = BuildSlotLease {
            slot: "slot-x".to_string(),
            agent: "AgentA".to_string(),
            branch: Some("feature".to_string()),
            exclusive: true,
            acquired_ts: now.to_rfc3339(),
            expires_ts: (now + chrono::Duration::hours(1)).to_rfc3339(),
            released_ts: None,
        };
        let conflict = BuildSlotLease {
            slot: "slot-x".to_string(),
            agent: "AgentB".to_string(),
            branch: Some("main".to_string()),
            exclusive: true,
            acquired_ts: now.to_rfc3339(),
            expires_ts: (now + chrono::Duration::hours(1)).to_rfc3339(),
            released_ts: None,
        };
        let response = AcquireBuildSlotResponse {
            granted,
            conflicts: vec![conflict],
        };
        let json = serde_json::to_string(&response).unwrap();
        let parsed: AcquireBuildSlotResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.granted.slot, "slot-x");
        assert_eq!(parsed.conflicts.len(), 1);
        assert_eq!(parsed.conflicts[0].agent, "AgentB");
    }

    #[test]
    fn renew_response_serde() {
        let response = RenewBuildSlotResponse {
            renewed: true,
            expires_ts: "2026-02-12T12:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        let parsed: RenewBuildSlotResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.renewed);
        assert!(parsed.expires_ts.contains("2026"));
    }

    #[test]
    fn release_response_serde() {
        let response = ReleaseBuildSlotResponse {
            released: true,
            released_at: "2026-02-12T13:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        let parsed: ReleaseBuildSlotResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.released);
        assert!(parsed.released_at.contains("2026"));
    }

    // -----------------------------------------------------------------------
    // read_active_leases edge cases (br-3h13.4.5)
    // -----------------------------------------------------------------------

    #[test]
    fn read_active_leases_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        let leases = read_active_leases(dir.path(), now).unwrap();
        assert!(leases.is_empty());
    }

    #[test]
    fn read_active_leases_non_json_files_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();

        std::fs::write(dir.path().join("readme.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("config.yaml"), "key: value").unwrap();

        let leases = read_active_leases(dir.path(), now).unwrap();
        assert!(leases.is_empty());
    }

    #[test]
    fn read_active_leases_expired_lease_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();

        let expired = BuildSlotLease {
            slot: "slot-expired".to_string(),
            agent: "agent-old".to_string(),
            branch: Some("main".to_string()),
            exclusive: true,
            acquired_ts: (now - chrono::Duration::hours(2)).to_rfc3339(),
            expires_ts: (now - chrono::Duration::hours(1)).to_rfc3339(), // expired
            released_ts: None,
        };
        std::fs::write(
            dir.path().join("expired.json"),
            serde_json::to_string(&expired).unwrap(),
        )
        .unwrap();

        let leases = read_active_leases(dir.path(), now).unwrap();
        assert!(leases.is_empty(), "expired lease should be excluded");
    }

    #[test]
    fn read_active_leases_malformed_json_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();

        std::fs::write(dir.path().join("malformed.json"), "{ not valid json }").unwrap();

        let error = read_active_leases(dir.path(), now).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("malformed.json"));
    }

    #[test]
    fn read_active_leases_nonexistent_directory() {
        let now = chrono::Utc::now();
        let nonexistent = std::path::Path::new("/nonexistent/path/that/does/not/exist");
        let leases = read_active_leases(nonexistent, now).unwrap();
        assert!(
            leases.is_empty(),
            "nonexistent directory should return empty vec"
        );
    }

    fn make_lease(agent: &str, branch: Option<&str>, exclusive: bool) -> BuildSlotLease {
        let now = chrono::Utc::now();
        BuildSlotLease {
            slot: "slot-a".to_string(),
            agent: agent.to_string(),
            branch: branch.map(str::to_string),
            exclusive,
            acquired_ts: now.to_rfc3339(),
            expires_ts: (now + chrono::Duration::minutes(30)).to_rfc3339(),
            released_ts: None,
        }
    }

    #[test]
    fn collect_slot_conflicts_exclusive_request_conflicts_with_shared_holder() {
        let active = vec![make_lease("BlueLake", Some("main"), false)];
        let branch = Some("feature".to_string());
        let conflicts = collect_slot_conflicts(active, "GreenPeak", branch.as_deref(), true);
        assert_eq!(
            conflicts.len(),
            1,
            "exclusive requests should conflict with active shared holders"
        );
    }

    #[test]
    fn collect_slot_conflicts_shared_request_conflicts_with_exclusive_holder() {
        let active = vec![make_lease("BlueLake", Some("main"), true)];
        let branch = Some("feature".to_string());
        let conflicts = collect_slot_conflicts(active, "GreenPeak", branch.as_deref(), false);
        assert_eq!(
            conflicts.len(),
            1,
            "shared requests should conflict with existing exclusive holders"
        );
    }

    #[test]
    fn collect_slot_conflicts_shared_request_ignores_shared_holder() {
        let active = vec![make_lease("BlueLake", Some("main"), false)];
        let branch = Some("feature".to_string());
        let conflicts = collect_slot_conflicts(active, "GreenPeak", branch.as_deref(), false);
        assert!(
            conflicts.is_empty(),
            "shared requests should not conflict with other shared holders"
        );
    }

    #[test]
    fn collect_slot_conflicts_ignores_same_agent_branch() {
        let active = vec![make_lease("BlueLake", Some("main"), true)];
        let branch = Some("main".to_string());
        let conflicts = collect_slot_conflicts(active, "BlueLake", branch.as_deref(), true);
        assert!(
            conflicts.is_empty(),
            "agent reacquiring the same branch lease should not self-conflict"
        );
    }

    #[test]
    fn collect_slot_conflicts_same_agent_different_branch_is_not_a_conflict() {
        let active = vec![make_lease("BlueLake", Some("main"), true)];
        let branch = Some("feature".to_string());
        let conflicts = collect_slot_conflicts(active, "BlueLake", branch.as_deref(), true);
        assert!(
            conflicts.is_empty(),
            "the same agent should not self-conflict after a branch change"
        );
    }

    // Real filesystem transactions used by the MCP tools, not mock leases.

    #[test]
    fn slot_lock_is_nonblocking_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let first = try_lock_slot(dir.path(), true).unwrap().unwrap();
        let error = try_lock_slot(dir.path(), true).unwrap_err();
        assert_eq!(
            error.raw_os_error(),
            fs2::lock_contended_error().raw_os_error()
        );
        drop(first);
        let second = try_lock_slot(dir.path(), true).unwrap().unwrap();
        assert!(dir.path().join(".slot.lock").is_file());
        drop(second);
        assert!(dir.path().join(".slot.lock").is_file());
    }

    #[test]
    fn different_slots_do_not_share_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let _first = try_lock_slot(&dir.path().join("first"), true).unwrap();
        let _second = try_lock_slot(&dir.path().join("second"), true).unwrap();
    }

    #[test]
    fn missing_slot_renew_and_release_do_not_create_directories() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("absent");
        assert!(
            !renew_slot_lease(&slot, "BlueLake", None, 600)
                .unwrap()
                .renewed
        );
        assert!(
            !release_slot_lease(&slot, "BlueLake", None)
                .unwrap()
                .released
        );
        assert!(!slot.exists());
    }

    #[test]
    fn busy_slot_rejects_all_mutations_without_changing_lease() {
        let dir = tempfile::tempdir().unwrap();
        acquire_slot_lease(dir.path(), "slot-a", "BlueLake", None, 3600, true).unwrap();
        let lease_path = lease_path_for_holder(dir.path(), "BlueLake", None);
        let before = std::fs::read(&lease_path).unwrap();
        let lock = try_lock_slot(dir.path(), false).unwrap().unwrap();

        assert!(acquire_slot_lease(dir.path(), "slot-a", "GreenPeak", None, 3600, true).is_err());
        assert!(renew_slot_lease(dir.path(), "BlueLake", None, 600).is_err());
        assert!(release_slot_lease(dir.path(), "BlueLake", None).is_err());
        assert_eq!(std::fs::read(&lease_path).unwrap(), before);
        assert!(!lease_path_for_holder(dir.path(), "GreenPeak", None).exists());

        drop(lock);
        assert!(
            renew_slot_lease(dir.path(), "BlueLake", None, 600)
                .unwrap()
                .renewed
        );
        assert!(
            release_slot_lease(dir.path(), "BlueLake", None)
                .unwrap()
                .released
        );
    }

    #[test]
    fn acquired_holder_is_visible_to_next_advisory_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_slot_lease(dir.path(), "slot-a", "BlueLake", None, 3600, true).unwrap();
        assert!(first.conflicts.is_empty());
        let second =
            acquire_slot_lease(dir.path(), "slot-a", "GreenPeak", None, 3600, true).unwrap();
        assert_eq!(second.conflicts.len(), 1);
        assert_eq!(second.conflicts[0].agent, "BlueLake");
        assert_eq!(second.granted.agent, "GreenPeak");
        assert_eq!(
            read_active_leases(dir.path(), chrono::Utc::now())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn renewal_after_release_does_not_resurrect_lease() {
        let dir = tempfile::tempdir().unwrap();
        acquire_slot_lease(
            dir.path(),
            "slot-a",
            "BlueLake",
            Some("main".to_string()),
            3600,
            true,
        )
        .unwrap();
        let released = release_slot_lease(dir.path(), "BlueLake", Some("feature")).unwrap();
        assert!(released.released);
        assert!(
            !renew_slot_lease(dir.path(), "BlueLake", Some("feature"), 600)
                .unwrap()
                .renewed
        );
        let repeated = release_slot_lease(dir.path(), "BlueLake", Some("feature")).unwrap();
        assert_eq!(repeated.released_at, released.released_at);
        let current: BuildSlotLease = serde_json::from_str(
            &std::fs::read_to_string(lease_path_for_holder(dir.path(), "BlueLake", None)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            current.released_ts.as_deref(),
            Some(released.released_at.as_str())
        );
    }

    #[test]
    fn invalid_slot_state_prevents_new_lease_and_releases_lock() {
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("existing.json");
        std::fs::write(&corrupt, "{ incomplete").unwrap();
        let error =
            acquire_slot_lease(dir.path(), "slot-a", "BlueLake", None, 3600, true).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!lease_path_for_holder(dir.path(), "BlueLake", None).exists());
        assert_eq!(std::fs::read_to_string(&corrupt).unwrap(), "{ incomplete");
        let _lock = try_lock_slot(dir.path(), false).unwrap().unwrap();
    }

    #[test]
    fn directory_read_failure_is_not_an_empty_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-directory");
        std::fs::write(&path, "occupied").unwrap();
        assert!(read_active_leases(&path, chrono::Utc::now()).is_err());
    }

    #[test]
    fn slot_errors_distinguish_retryable_contention_from_invalid_state() {
        let busy = slot_io_error(fs2::lock_contended_error());
        let data = busy.data.as_ref().unwrap();
        assert_eq!(data["error"]["type"], "BUILD_SLOT_BUSY");
        assert_eq!(data["error"]["recoverable"], true);

        let invalid = slot_io_error(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "malformed lease",
        ));
        let data = invalid.data.as_ref().unwrap();
        assert_eq!(data["error"]["type"], "BUILD_SLOT_STATE_INVALID");
        assert_eq!(data["error"]["recoverable"], false);
    }

    #[test]
    fn lifecycle_admission_preserves_release_and_checks_imported_tombstones() {
        let mut agent = mcp_agent_mail_db::AgentRow {
            id: Some(81),
            project_id: 71,
            name: "BlueLake".into(),
            ..Default::default()
        };
        for operation in [
            SlotOperation::Acquire,
            SlotOperation::Renew,
            SlotOperation::Release,
        ] {
            enforce_slot_lifecycle(&agent, operation).unwrap();
        }
        agent.retired_at = Some(5_000_000);
        for operation in [SlotOperation::Acquire, SlotOperation::Renew] {
            let error = enforce_slot_lifecycle(&agent, operation).unwrap_err();
            assert_eq!(crate::tool_error_code(&error), Some("AGENT_RETIRED"));
        }
        enforce_slot_lifecycle(&agent, SlotOperation::Release).unwrap();
        for retired_at in [None, Some(5_000_000)] {
            agent.retired_at = retired_at;
            agent.task_description =
                "[DEREGISTERED at 2026-10-01 00:00:00] imported identity".into();
            for operation in [SlotOperation::Acquire, SlotOperation::Renew] {
                let error = enforce_slot_lifecycle(&agent, operation).unwrap_err();
                assert_eq!(crate::tool_error_code(&error), Some("AGENT_DEREGISTERED"));
            }
            enforce_slot_lifecycle(&agent, SlotOperation::Release).unwrap();
        }
    }

    fn stored_lease(path: &Path) -> BuildSlotLease {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn renewal_updates_active_alias_not_expired_preferred_file() {
        let dir = tempfile::tempdir().unwrap();
        let preferred = lease_path_for_holder(dir.path(), "BlueLake", None);
        let alias = legacy_lease_path_for_holder(dir.path(), "BlueLake", Some("old-branch"));
        let mut expired = make_lease("BlueLake", Some("main"), true);
        expired.expires_ts = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        write_lease_json(&preferred, &expired).unwrap();
        let untouched = std::fs::read(&preferred).unwrap();
        let active = make_lease("BlueLake", Some("old-branch"), true);
        let expected = compute_renewed_expiry(chrono::Utc::now(), &active.expires_ts, 600);
        write_lease_json(&alias, &active).unwrap();

        let result = renew_slot_lease(dir.path(), "BlueLake", Some("new-branch"), 600).unwrap();

        assert!(result.renewed);
        assert_eq!(result.expires_ts, expected);
        assert_eq!(stored_lease(&alias).expires_ts, expected);
        assert_eq!(stored_lease(&alias).branch, active.branch);
        assert_eq!(std::fs::read(&preferred).unwrap(), untouched);
    }

    #[test]
    fn release_updates_active_alias_and_removes_its_advisory_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let preferred = lease_path_for_holder(dir.path(), "BlueLake", None);
        let alias = legacy_lease_path_for_holder(dir.path(), "BlueLake", Some("old-branch"));
        let mut expired = make_lease("BlueLake", None, true);
        expired.expires_ts = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        write_lease_json(&preferred, &expired).unwrap();
        let untouched = std::fs::read(&preferred).unwrap();
        write_lease_json(&alias, &make_lease("BlueLake", Some("old-branch"), true)).unwrap();

        let result = release_slot_lease(dir.path(), "BlueLake", Some("new-branch")).unwrap();

        assert!(result.released);
        assert_eq!(stored_lease(&alias).released_ts, Some(result.released_at));
        assert_eq!(std::fs::read(&preferred).unwrap(), untouched);
        let next =
            acquire_slot_lease(dir.path(), "slot-a", "GreenPeak", None, 3600, true).unwrap();
        assert!(
            next.conflicts.is_empty(),
            "released legacy lease still blocks another holder"
        );
    }

    #[test]
    fn release_settles_every_owned_alias_and_preserves_other_holders() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("branch-one.json");
        let second = dir.path().join("branch-two.json");
        let peer = dir.path().join("peer.json");
        write_lease_json(&first, &make_lease("BlueLake", Some("one"), true)).unwrap();
        write_lease_json(&second, &make_lease("BlueLake", Some("two"), false)).unwrap();
        write_lease_json(&peer, &make_lease("GreenPeak", None, true)).unwrap();
        let peer_before = std::fs::read(&peer).unwrap();

        let result = release_slot_lease(dir.path(), "BlueLake", Some("three")).unwrap();

        assert!(result.released);
        for path in [&first, &second] {
            assert_eq!(
                stored_lease(path).released_ts.as_deref(),
                Some(result.released_at.as_str())
            );
        }
        assert_eq!(std::fs::read(&peer).unwrap(), peer_before);
        let first_after = std::fs::read(&first).unwrap();
        let second_after = std::fs::read(&second).unwrap();
        let replay = release_slot_lease(dir.path(), "BlueLake", None).unwrap();
        assert_eq!(replay.released_at, result.released_at);
        assert_eq!(std::fs::read(&first).unwrap(), first_after);
        assert_eq!(std::fs::read(&second).unwrap(), second_after);
        assert!(!lease_path_for_holder(dir.path(), "BlueLake", None).exists());
    }

    #[test]
    fn renewal_extends_all_active_aliases_to_one_nonshortening_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("one.json");
        let second = dir.path().join("two.json");
        let released_path = dir.path().join("released.json");
        let peer = dir.path().join("peer.json");
        let short = make_lease("BlueLake", Some("one"), true);
        let mut long = make_lease("BlueLake", Some("two"), false);
        long.expires_ts = (chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339();
        let mut released = make_lease("BlueLake", None, true);
        released.released_ts = Some(chrono::Utc::now().to_rfc3339());
        let expected = compute_renewed_expiry(chrono::Utc::now(), &long.expires_ts, 600);
        for (path, lease) in [
            (&first, &short),
            (&second, &long),
            (&released_path, &released),
        ] {
            write_lease_json(path, lease).unwrap();
        }
        write_lease_json(&peer, &make_lease("GreenPeak", None, true)).unwrap();
        let released_before = std::fs::read(&released_path).unwrap();
        let peer_before = std::fs::read(&peer).unwrap();

        let result = renew_slot_lease(dir.path(), "BlueLake", Some("changed"), 600).unwrap();

        assert!(result.renewed);
        assert_eq!(result.expires_ts, expected);
        assert_eq!(stored_lease(&first).expires_ts, expected);
        assert_eq!(stored_lease(&second).expires_ts, expected);
        assert_eq!(stored_lease(&first).branch, short.branch);
        assert_eq!(stored_lease(&second).branch, long.branch);
        assert_eq!(std::fs::read(&released_path).unwrap(), released_before);
        assert_eq!(std::fs::read(&peer).unwrap(), peer_before);
    }

    #[test]
    fn release_resumes_after_an_already_released_alias_without_short_circuiting() {
        let dir = tempfile::tempdir().unwrap();
        let completed = dir.path().join("a-completed.json");
        let pending = dir.path().join("z-pending.json");
        let mut old = make_lease("BlueLake", Some("one"), true);
        old.released_ts = Some("2000-01-01T00:00:00+00:00".into());
        write_lease_json(&completed, &old).unwrap();
        write_lease_json(&pending, &make_lease("BlueLake", Some("two"), true)).unwrap();
        let completed_before = std::fs::read(&completed).unwrap();

        let result = release_slot_lease(dir.path(), "BlueLake", None).unwrap();

        assert!(result.released);
        assert_eq!(
            stored_lease(&pending).released_ts.as_deref(),
            Some(result.released_at.as_str())
        );
        assert_eq!(std::fs::read(&completed).unwrap(), completed_before);
        assert_eq!(
            release_slot_lease(dir.path(), "BlueLake", None)
                .unwrap()
                .released_at,
            result.released_at,
        );
        assert!(
            !renew_slot_lease(dir.path(), "BlueLake", None, 600)
                .unwrap()
                .renewed
        );
    }

    #[test]
    fn existing_filename_never_redirects_renewal_or_release_into_another_holder() {
        for release in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let foreign = lease_path_for_holder(dir.path(), "BlueLake", None);
            let owned = dir.path().join("actual-owned-lease.json");
            write_lease_json(&foreign, &make_lease("GreenPeak", None, true)).unwrap();
            write_lease_json(&owned, &make_lease("BlueLake", Some("old"), true)).unwrap();
            let foreign_before = std::fs::read(&foreign).unwrap();
            let owned_before = std::fs::read(&owned).unwrap();
            if release {
                assert!(
                    release_slot_lease(dir.path(), "BlueLake", None)
                        .unwrap()
                        .released
                );
                assert!(stored_lease(&owned).released_ts.is_some());
            } else {
                assert!(
                    renew_slot_lease(dir.path(), "BlueLake", None, 600)
                        .unwrap()
                        .renewed
                );
            }
            assert_eq!(std::fs::read(&foreign).unwrap(), foreign_before);
            assert_ne!(std::fs::read(&owned).unwrap(), owned_before);
        }
    }

    #[test]
    fn alias_mutations_validate_the_complete_inventory_before_any_write() {
        for release in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let owned = dir.path().join("owned.json");
            write_lease_json(&owned, &make_lease("BlueLake", None, true)).unwrap();
            let original = std::fs::read(&owned).unwrap();
            let invalid = dir.path().join("invalid.json");
            std::fs::write(&invalid, "{ interrupted lease").unwrap();
            let error = if release {
                release_slot_lease(dir.path(), "BlueLake", None).unwrap_err()
            } else {
                renew_slot_lease(dir.path(), "BlueLake", None, 600).unwrap_err()
            };
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert_eq!(std::fs::read(&owned).unwrap(), original);
            assert_eq!(
                std::fs::read_to_string(&invalid).unwrap(),
                "{ interrupted lease"
            );
            let _lock = try_lock_slot(dir.path(), false).unwrap().unwrap();
        }
    }

    #[test]
    fn public_build_slot_calls_reach_the_inventoried_alias() {
        lifecycle_mailbox(|ctx, _, config, key| {
            let root = project_archive_root(config, "slot-lifecycle");
            let slot = slot_dir(&root, "aliases");
            let preferred = lease_path_for_holder(&slot, "BlueLake", None);
            let alias = legacy_lease_path_for_holder(&slot, "BlueLake", Some("old"));
            let mut expired = make_lease("BlueLake", None, true);
            expired.expires_ts =
                (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
            write_lease_json(&preferred, &expired).unwrap();
            write_lease_json(&alias, &make_lease("BlueLake", Some("old"), true)).unwrap();
            let preferred_before = std::fs::read(&preferred).unwrap();
            crate::session_identity::bind(ctx, 71, 81, "BlueLake");
            fastmcp_core::block_on(async {
                let result = renew_build_slot(
                    ctx,
                    key.into(),
                    "BlueLake".into(),
                    "aliases".into(),
                    Some(600),
                )
                .await
                .unwrap();
                let result: RenewBuildSlotResponse = serde_json::from_str(&result).unwrap();
                assert!(result.renewed);
                assert_eq!(stored_lease(&alias).expires_ts, result.expires_ts);
                let result =
                    release_build_slot(ctx, key.into(), "BlueLake".into(), "aliases".into())
                        .await
                        .unwrap();
                let result: ReleaseBuildSlotResponse = serde_json::from_str(&result).unwrap();
                assert!(result.released);
                assert_eq!(stored_lease(&alias).released_ts, Some(result.released_at));
            });
            assert_eq!(std::fs::read(&preferred).unwrap(), preferred_before);
        });
    }

    fn lifecycle_mailbox(
        test: impl FnOnce(&McpContext, &mcp_agent_mail_db::DbPool, &Config, &str),
    ) {
        let temp = tempfile::tempdir().unwrap();
        let storage_root = temp.path().join("archive");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&storage_root).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let database_url =
            mcp_agent_mail_core::disk::sqlite_url_from_path(&temp.path().join("mail.sqlite3"));
        let root = storage_root.to_string_lossy().into_owned();
        let key = project.to_string_lossy().into_owned();
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", database_url.as_str()),
                ("STORAGE_ROOT", root.as_str()),
                ("WORKTREES_ENABLED", "true"),
                ("MESSAGING_SESSION_IDENTITY", "true"),
            ],
            || {
                Config::reset_cached();
                let config = Config::get();
                let pool = get_db_pool().unwrap();
                let ctx = McpContext::with_state(
                    asupersync::Cx::for_testing(),
                    1,
                    fastmcp_core::SessionState::new(),
                );
                let conn = fastmcp_core::block_on(pool.acquire(ctx.cx()))
                    .into_result()
                    .unwrap();
                let escaped_key = key.replace('\'', "''");
                conn.execute_raw(&format!(
                    "INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'slot-lifecycle', '{escaped_key}', 1)"
                )).unwrap();
                conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 'active', 1, 1), (82, 71, 'RedStone', 'test', 'test', '[DEREGISTERED at 2026-10-01 00:00:00] imported identity', 1, 1)").unwrap();
                drop(conn);
                test(&ctx, &pool, &config, &key);
                drop(pool);
                Config::reset_cached();
            },
        );
    }

    #[test]
    fn public_slots_refuse_retired_work_preserve_release_and_resume_after_unretire() {
        lifecycle_mailbox(|ctx, pool, config, key| {
            fastmcp_core::block_on(async {
                crate::session_identity::bind(ctx, 71, 81, "BlueLake");
                let acquired = acquire_build_slot(
                    ctx,
                    key.into(),
                    "BlueLake".into(),
                    "existing".into(),
                    Some(3600),
                    None,
                )
                .await
                .unwrap();
                let acquired: AcquireBuildSlotResponse = serde_json::from_str(&acquired).unwrap();
                assert_eq!(acquired.granted.agent, "BlueLake");
                let root = project_archive_root(config, "slot-lifecycle");
                let existing = slot_dir(&root, "existing");
                let path = lease_path_for_holder(&existing, "BlueLake", None);
                let before = std::fs::read(&path).unwrap();
                db_outcome_to_mcp_result(
                    mcp_agent_mail_db::queries::set_agent_retired_at(
                        ctx.cx(),
                        pool,
                        81,
                        Some(mcp_agent_mail_db::now_micros()),
                    )
                    .await,
                )
                .unwrap();
                let acquire_error = acquire_build_slot(
                    ctx,
                    key.into(),
                    "BlueLake".into(),
                    "new-work".into(),
                    None,
                    None,
                )
                .await
                .unwrap_err();
                let renew_error =
                    renew_build_slot(ctx, key.into(), "BlueLake".into(), "existing".into(), None)
                        .await
                        .unwrap_err();
                assert_eq!(
                    crate::tool_error_code(&acquire_error),
                    Some("AGENT_RETIRED")
                );
                assert_eq!(crate::tool_error_code(&renew_error), Some("AGENT_RETIRED"));
                assert_eq!(std::fs::read(&path).unwrap(), before);
                assert!(!slot_dir(&root, "new-work").exists());
                let released =
                    release_build_slot(ctx, key.into(), "BlueLake".into(), "existing".into())
                        .await
                        .unwrap();
                assert!(
                    serde_json::from_str::<ReleaseBuildSlotResponse>(&released)
                        .unwrap()
                        .released
                );
                db_outcome_to_mcp_result(
                    mcp_agent_mail_db::queries::set_agent_retired_at(ctx.cx(), pool, 81, None)
                        .await,
                )
                .unwrap();
                let reacquired = acquire_build_slot(
                    ctx,
                    key.into(),
                    "BlueLake".into(),
                    "existing".into(),
                    None,
                    None,
                )
                .await
                .unwrap();
                assert!(
                    serde_json::from_str::<AcquireBuildSlotResponse>(&reacquired)
                        .unwrap()
                        .granted
                        .released_ts
                        .is_none()
                );
                let renewed =
                    renew_build_slot(ctx, key.into(), "BlueLake".into(), "existing".into(), None)
                        .await
                        .unwrap();
                assert!(
                    serde_json::from_str::<RenewBuildSlotResponse>(&renewed)
                        .unwrap()
                        .renewed
                );
            });
        });
    }

    #[test]
    fn public_slots_recognize_imported_deregistration_without_retirement_timestamp() {
        lifecycle_mailbox(|ctx, pool, config, key| {
            let root = project_archive_root(config, "slot-lifecycle");
            let existing = slot_dir(&root, "imported");
            acquire_slot_lease(&existing, "imported", "RedStone", None, 3600, true).unwrap();
            let path = lease_path_for_holder(&existing, "RedStone", None);
            let before = std::fs::read(&path).unwrap();
            fastmcp_core::block_on(async {
                let acquire_error = acquire_build_slot(
                    ctx,
                    key.into(),
                    "RedStone".into(),
                    "new-work".into(),
                    None,
                    None,
                )
                .await
                .unwrap_err();
                let renew_error =
                    renew_build_slot(ctx, key.into(), "RedStone".into(), "imported".into(), None)
                        .await
                        .unwrap_err();
                assert_eq!(
                    crate::tool_error_code(&acquire_error),
                    Some("AGENT_DEREGISTERED")
                );
                assert_eq!(
                    crate::tool_error_code(&renew_error),
                    Some("AGENT_DEREGISTERED")
                );
                assert_eq!(std::fs::read(&path).unwrap(), before);
                assert!(!slot_dir(&root, "new-work").exists());
                let released =
                    release_build_slot(ctx, key.into(), "RedStone".into(), "imported".into())
                        .await
                        .unwrap();
                assert!(
                    serde_json::from_str::<ReleaseBuildSlotResponse>(&released)
                        .unwrap()
                        .released
                );
                let row = db_outcome_to_mcp_result(
                    mcp_agent_mail_db::queries::get_agent(ctx.cx(), pool, 71, "RedStone").await,
                )
                .unwrap();
                assert!(row.retired_at.is_none());
                assert!(row.task_description.starts_with("[DEREGISTERED "));
            });
        });
    }

    #[test]
    fn public_slot_release_does_not_bypass_session_authority_for_inactive_peers() {
        lifecycle_mailbox(|ctx, _, config, key| {
            let root = project_archive_root(config, "slot-lifecycle");
            let existing = slot_dir(&root, "peer");
            acquire_slot_lease(&existing, "peer", "RedStone", None, 3600, true).unwrap();
            let path = lease_path_for_holder(&existing, "RedStone", None);
            let before = std::fs::read(&path).unwrap();
            crate::session_identity::bind(ctx, 71, 81, "BlueLake");
            fastmcp_core::block_on(async {
                let acquire_error = acquire_build_slot(
                    ctx,
                    key.into(),
                    "RedStone".into(),
                    "new-work".into(),
                    None,
                    None,
                )
                .await
                .unwrap_err();
                let release_error =
                    release_build_slot(ctx, key.into(), "RedStone".into(), "peer".into())
                        .await
                        .unwrap_err();
                assert_eq!(
                    crate::tool_error_code(&acquire_error),
                    Some("SESSION_IDENTITY_MISMATCH")
                );
                assert_eq!(
                    crate::tool_error_code(&release_error),
                    Some("SESSION_IDENTITY_MISMATCH")
                );
            });
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(!slot_dir(&root, "new-work").exists());
        });
    }

    #[test]
    fn public_slots_preserve_trusted_local_unregistered_callers() {
        lifecycle_mailbox(|ctx, pool, _, key| {
            fastmcp_core::block_on(async {
                let acquired = acquire_build_slot(
                    ctx,
                    key.into(),
                    "GreenCastle".into(),
                    "unregistered".into(),
                    None,
                    None,
                )
                .await
                .unwrap();
                assert_eq!(
                    serde_json::from_str::<AcquireBuildSlotResponse>(&acquired)
                        .unwrap()
                        .granted
                        .agent,
                    "GreenCastle"
                );
                let renewed = renew_build_slot(
                    ctx,
                    key.into(),
                    "GreenCastle".into(),
                    "unregistered".into(),
                    None,
                )
                .await
                .unwrap();
                assert!(
                    serde_json::from_str::<RenewBuildSlotResponse>(&renewed)
                        .unwrap()
                        .renewed
                );
                let released = release_build_slot(
                    ctx,
                    key.into(),
                    "GreenCastle".into(),
                    "unregistered".into(),
                )
                .await
                .unwrap();
                assert!(
                    serde_json::from_str::<ReleaseBuildSlotResponse>(&released)
                        .unwrap()
                        .released
                );
                assert!(matches!(
                    mcp_agent_mail_db::queries::get_agent(ctx.cx(), pool, 71, "GreenCastle").await,
                    asupersync::Outcome::Err(mcp_agent_mail_db::DbError::NotFound { .. }),
                ));
            });
        });
    }
}
