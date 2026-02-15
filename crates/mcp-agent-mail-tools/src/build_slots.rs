//! Build slot cluster tools (coarse concurrency control)
//!
//! Ported from legacy Python:
//! - Only meaningful when `WORKTREES_ENABLED=1`
//! - Stores per-slot leases as JSON files under the per-project archive root:
//!   `{storage_root}/projects/{project_slug}/build_slots/{slot}/{agent__branch}.json`
//! - Conflicts are detected by scanning active (non-expired) leases.

use fastmcp::prelude::*;
use mcp_agent_mail_core::Config;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::tool_util::{get_db_pool, legacy_tool_error, resolve_project};

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
    let mut safe = value.trim().to_string();
    for ch in ['/', '\\', ':', '*', '?', '"', '<', '>', '|', ' '] {
        safe = safe.replace(ch, "_");
    }
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
            continue;
        };
        let Ok(lease) = serde_json::from_str::<BuildSlotLease>(&text) else {
            continue;
        };
        let Ok(exp) = chrono::DateTime::parse_from_rfc3339(&lease.expires_ts) else {
            // Ignore malformed leases: invalid expiration should not block slots forever.
            continue;
        };
        if exp.with_timezone(&chrono::Utc) <= now {
            continue;
        }
        results.push(lease);
    }
    results
}

fn write_lease_json(path: &Path, lease: &BuildSlotLease) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text =
        serde_json::to_string_pretty(lease).map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(path, text)
}

fn worktrees_required() -> McpError {
    legacy_tool_error(
        "FEATURE_DISABLED",
        "Build slots are disabled. Enable WORKTREES_ENABLED to use this tool.",
        false,
        serde_json::json!({ "feature": "worktrees", "env_var": "WORKTREES_ENABLED" }),
    )
}

/// Acquire a build slot (advisory), optionally exclusive. Returns conflicts when another holder is active.
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

    let now = chrono::Utc::now();
    let ttl = std::cmp::max(ttl_seconds.unwrap_or(3600), 60);
    let expires_ts = (now + chrono::Duration::seconds(ttl)).to_rfc3339();
    let branch = compute_branch(&project.human_key);
    let is_exclusive = exclusive.unwrap_or(true);

    let project_root = project_archive_root(config, &project.slug);
    let slot_path = slot_dir(&project_root, &slot);
    std::fs::create_dir_all(&slot_path)
        .map_err(|e| McpError::internal_error(format!("failed to create slot dir: {e}")))?;

    let active = read_active_leases(&slot_path, now);
    let mut conflicts = Vec::new();
    if is_exclusive {
        for entry in active {
            if entry.agent == agent_name && entry.branch == branch {
                continue;
            }
            if entry.exclusive {
                conflicts.push(entry);
            }
        }
    }

    let holder_id = safe_component(&format!(
        "{agent_name}__{}",
        branch.clone().unwrap_or_else(|| "unknown".to_string())
    ));
    let lease_path = slot_path.join(format!("{holder_id}.json"));

    let granted = BuildSlotLease {
        slot: slot.clone(),
        agent: agent_name.clone(),
        branch,
        exclusive: is_exclusive,
        acquired_ts: now.to_rfc3339(),
        expires_ts,
        released_ts: None,
    };

    // Best-effort write, matching legacy behavior.
    let _ = write_lease_json(&lease_path, &granted);

    let response = AcquireBuildSlotResponse { granted, conflicts };
    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Extend expiry for an existing build slot lease. No-op if missing.
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

    let now = chrono::Utc::now();
    let extend = std::cmp::max(extend_seconds.unwrap_or(1800), 60);
    let new_exp = (now + chrono::Duration::seconds(extend)).to_rfc3339();

    let project_root = project_archive_root(config, &project.slug);
    let slot_path = slot_dir(&project_root, &slot);

    let branch = compute_branch(&project.human_key);
    let holder_id = safe_component(&format!(
        "{agent_name}__{}",
        branch.clone().unwrap_or_else(|| "unknown".to_string())
    ));
    let lease_path = slot_path.join(format!("{holder_id}.json"));

    let mut current = std::fs::read_to_string(&lease_path)
        .ok()
        .and_then(|t| serde_json::from_str::<BuildSlotLease>(&t).ok())
        .unwrap_or_else(|| BuildSlotLease {
            slot: slot.clone(),
            agent: agent_name.clone(),
            branch: branch.clone(),
            exclusive: true,
            acquired_ts: now.to_rfc3339(),
            expires_ts: new_exp.clone(),
            released_ts: None,
        });

    current.slot = slot;
    current.agent = agent_name;
    current.branch = branch;
    current.expires_ts.clone_from(&new_exp);
    let _ = write_lease_json(&lease_path, &current);

    let response = RenewBuildSlotResponse {
        renewed: true,
        expires_ts: new_exp,
    };
    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Mark an active slot lease as released (non-destructive; keeps JSON with `released_ts`).
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

    let now = chrono::Utc::now();
    let now_iso = now.to_rfc3339();

    let project_root = project_archive_root(config, &project.slug);
    let slot_path = slot_dir(&project_root, &slot);
    let branch = compute_branch(&project.human_key);
    let holder_id = safe_component(&format!(
        "{agent_name}__{}",
        branch.clone().unwrap_or_else(|| "unknown".to_string())
    ));
    let lease_path = slot_path.join(format!("{holder_id}.json"));

    let mut released = false;
    let mut data = std::fs::read_to_string(&lease_path)
        .ok()
        .and_then(|t| serde_json::from_str::<BuildSlotLease>(&t).ok())
        .unwrap_or_else(|| BuildSlotLease {
            slot,
            agent: agent_name,
            branch,
            exclusive: true,
            acquired_ts: now_iso.clone(),
            expires_ts: now_iso.clone(),
            released_ts: None,
        });

    data.released_ts = Some(now_iso.clone());
    data.expires_ts.clone_from(&now_iso);
    if write_lease_json(&lease_path, &data).is_ok() {
        released = true;
    }

    let response = ReleaseBuildSlotResponse {
        released,
        released_at: now_iso,
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
}
