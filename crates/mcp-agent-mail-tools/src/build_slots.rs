//! Build slot cluster tools (coarse concurrency control)
//!
//! Ported from legacy Python:
//! - Only meaningful when `WORKTREES_ENABLED=1`
//! - Stores per-slot leases as JSON files under the per-project archive root:
//!   `{storage_root}/projects/{project_slug}/build_slots/{slot}/{agent_hash}.json`
//! - Conflicts are detected by scanning active (non-expired) leases.

use fastmcp::prelude::*;
use mcp_agent_mail_core::Config;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tool_util::{get_db_pool, legacy_tool_error, resolve_project};

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
    // A slot holds at most one active lease per agent (see
    // `existing_agent_lease_path` and `collect_slot_conflicts`), so renew/
    // release must match by agent identity only — the recorded branch on the
    // lease can drift when the project's current branch changes after
    // acquire, and strict equality would silently turn renew/release into
    // no-ops.
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
    head.shorthand().map(str::to_string)
}

fn read_active_leases(slot_path: &Path, now: chrono::DateTime<chrono::Utc>) -> Vec<BuildSlotLease> {
    let mut results = Vec::new();
    let Ok(entries) = std::fs::read_dir(slot_path) else {
        return results;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            tracing::warn!(path = %path.display(), "ignoring unreadable build slot lease file");
            continue;
        };
        let Ok(lease) = serde_json::from_str::<BuildSlotLease>(&text) else {
            tracing::warn!(path = %path.display(), "ignoring malformed build slot lease file");
            continue;
        };
        let Ok(exp) = chrono::DateTime::parse_from_rfc3339(&lease.expires_ts) else {
            // Ignore malformed leases: invalid expiration should not block slots forever.
            continue;
        };
        if exp.with_timezone(&chrono::Utc) <= now {
            // Lease expired. We do not delete the file here to prevent a TOCTOU race
            // where another agent just renewed it. The file will be overwritten on next acquire.
            continue;
        }
        results.push(lease);
    }
    results
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

async fn resolve_canonical_agent_name(
    ctx: &McpContext,
    pool: &mcp_agent_mail_db::pool::DbPool,
    project_id: i64,
    agent_name: &str,
) -> String {
    let agent_name = mcp_agent_mail_core::models::normalize_agent_name(agent_name)
        .unwrap_or_else(|| agent_name.to_string());
    match mcp_agent_mail_db::queries::get_agent(ctx.cx(), pool, project_id, &agent_name).await {
        asupersync::Outcome::Ok(agent) => agent.name,
        _ => agent_name,
    }
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
        let conflicts_with_entry = if request_exclusive {
            true
        } else {
            entry.exclusive
        };
        if conflicts_with_entry {
            conflicts.push(entry);
        }
    }
    conflicts
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
    let agent_name = resolve_canonical_agent_name(ctx, &pool, project_id, &agent_name).await;

    let now = chrono::Utc::now();
    let ttl = ttl_seconds.map_or(3600, |t| t.clamp(60, 31_536_000)); // 1 hour default
    let expires_ts = (now + chrono::Duration::seconds(ttl)).to_rfc3339();
    let branch = compute_branch(&project.human_key);
    let is_exclusive = exclusive.unwrap_or(true);

    let project_root = project_archive_root(config, &project.slug);
    let slot_path = slot_dir(&project_root, &slot);
    std::fs::create_dir_all(&slot_path)
        .map_err(|e| McpError::internal_error(format!("failed to create slot dir: {e}")))?;

    let active = read_active_leases(&slot_path, now);
    let conflicts = collect_slot_conflicts(active, &agent_name, branch.as_deref(), is_exclusive);

    let lease_path = resolve_holder_lease_path(&slot_path, &agent_name, branch.as_deref());

    let granted = BuildSlotLease {
        slot: slot.clone(),
        agent: agent_name,
        branch,
        exclusive: is_exclusive,
        acquired_ts: now.to_rfc3339(),
        expires_ts,
        released_ts: None,
    };

    write_lease_json(&lease_path, &granted).map_err(|e| {
        McpError::internal_error(format!("failed to persist build slot lease: {e}"))
    })?;

    let response = AcquireBuildSlotResponse { granted, conflicts };
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
    let agent_name = resolve_canonical_agent_name(ctx, &pool, project_id, &agent_name).await;

    let now = chrono::Utc::now();
    let extend = extend_seconds.map_or(1800, |t| t.clamp(60, 31_536_000)); // 30 minutes default
    let branch = compute_branch(&project.human_key);

    let project_root = project_archive_root(config, &project.slug);
    let slot_path = slot_dir(&project_root, &slot);

    let active = read_active_leases(&slot_path, now);

    for mut current in active {
        if lease_matches_holder(&current, &agent_name, branch.as_deref())
            && current.released_ts.is_none()
        {
            let lease_path =
                resolve_holder_lease_path(&slot_path, &current.agent, current.branch.as_deref());
            let new_exp = compute_renewed_expiry(now, &current.expires_ts, extend);
            current.expires_ts.clone_from(&new_exp);
            write_lease_json(&lease_path, &current).map_err(|e| {
                McpError::internal_error(format!("failed to persist renewed build slot lease: {e}"))
            })?;
            return serde_json::to_string(&RenewBuildSlotResponse {
                renewed: true,
                expires_ts: new_exp,
            })
            .map_err(|e| McpError::internal_error(format!("JSON error: {e}")));
        }
    }
    let response = RenewBuildSlotResponse {
        renewed: false,
        expires_ts: String::new(),
    };
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
    let agent_name = resolve_canonical_agent_name(ctx, &pool, project_id, &agent_name).await;

    let now = chrono::Utc::now();
    let now_iso = now.to_rfc3339();
    let branch = compute_branch(&project.human_key);

    let project_root = project_archive_root(config, &project.slug);
    let slot_path = slot_dir(&project_root, &slot);

    let mut released = false;
    let mut released_at = String::new();
    let active = read_active_leases(&slot_path, now);

    for mut lease in active {
        if lease_matches_holder(&lease, &agent_name, branch.as_deref()) {
            if let Some(existing_release) = lease.released_ts.clone() {
                released = true;
                released_at = existing_release;
                break;
            }
            let lease_path =
                resolve_holder_lease_path(&slot_path, &lease.agent, lease.branch.as_deref());

            released_at.clone_from(&now_iso);
            lease.released_ts = Some(now_iso);
            write_lease_json(&lease_path, &lease).map_err(|e| {
                McpError::internal_error(format!(
                    "failed to persist released build slot lease: {e}"
                ))
            })?;
            released = true;
            break;
        }
    }

    let response = ReleaseBuildSlotResponse {
        released,
        released_at,
    };
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
    fn read_active_leases_ignores_invalid_expiration() {
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

        let leases = read_active_leases(dir.path(), now);
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].agent, "agent-valid");
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
        let leases = read_active_leases(dir.path(), now);
        assert!(leases.is_empty());
    }

    #[test]
    fn read_active_leases_non_json_files_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();

        std::fs::write(dir.path().join("readme.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("config.yaml"), "key: value").unwrap();

        let leases = read_active_leases(dir.path(), now);
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

        let leases = read_active_leases(dir.path(), now);
        assert!(leases.is_empty(), "expired lease should be excluded");
    }

    #[test]
    fn read_active_leases_malformed_json_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();

        std::fs::write(dir.path().join("malformed.json"), "{ not valid json }").unwrap();

        let leases = read_active_leases(dir.path(), now);
        assert!(
            leases.is_empty(),
            "malformed JSON should be silently ignored"
        );
    }

    #[test]
    fn read_active_leases_nonexistent_directory() {
        let now = chrono::Utc::now();
        let nonexistent = std::path::Path::new("/nonexistent/path/that/does/not/exist");
        let leases = read_active_leases(nonexistent, now);
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
}
