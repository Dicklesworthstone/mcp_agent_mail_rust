//! File reservation cluster tools
//!
//! Tools for advisory file locking:
//! - `file_reservation_paths`: Request file reservations
//! - `release_file_reservations`: Release reservations
//! - `renew_file_reservations`: Extend reservation TTL
//! - `force_release_file_reservation`: Force release stale reservation
//! - `install_precommit_guard`: Install Git pre-commit hook
//! - `uninstall_precommit_guard`: Remove pre-commit hook

use fastmcp::McpErrorCode;
use fastmcp::prelude::*;
use globset::Glob;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::micros_to_iso;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;

use crate::tool_util::{db_outcome_to_mcp_result, get_db_pool, resolve_agent, resolve_project};

/// Granted reservation record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantedReservation {
    pub id: i64,
    pub path_pattern: String,
    pub exclusive: bool,
    pub reason: String,
    pub expires_ts: String,
}

/// Conflict record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservationConflict {
    pub path: String,
    pub holders: Vec<ConflictHolder>,
}

/// Conflict holder info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictHolder {
    pub agent_name: String,
    pub reservation_id: i64,
    pub expires_ts: String,
}

fn normalize_pattern(pattern: &str) -> String {
    let mut normalized = pattern.trim().replace('\\', "/");
    while normalized.starts_with("./") {
        normalized = normalized[2..].to_string();
    }
    normalized.trim_start_matches('/').to_string()
}

fn patterns_overlap(a: &str, b: &str) -> bool {
    let a_norm = normalize_pattern(a);
    let b_norm = normalize_pattern(b);

    let a_glob = Glob::new(&a_norm).ok().map(|g| g.compile_matcher());
    let b_glob = Glob::new(&b_norm).ok().map(|g| g.compile_matcher());

    if let (Some(a_matcher), Some(b_matcher)) = (a_glob, b_glob) {
        let a_matches_b = a_matcher.is_match(&b_norm);
        let b_matches_a = b_matcher.is_match(&a_norm);
        return a_matches_b || b_matches_a || a_norm == b_norm;
    }

    a_norm == b_norm
}

/// File reservation response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservationResponse {
    pub granted: Vec<GrantedReservation>,
    pub conflicts: Vec<ReservationConflict>,
}

/// Release result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseResult {
    pub released: i32,
    pub released_at: String,
}

/// Renewal result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewalResult {
    pub renewed: i32,
    pub file_reservations: Vec<RenewedReservation>,
}

/// Renewed reservation info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewedReservation {
    pub id: i64,
    pub path_pattern: String,
    pub old_expires_ts: String,
    pub new_expires_ts: String,
}

fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home);
        }
        return PathBuf::from(input);
    }
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(input)
}

fn normalize_repo_path(input: &str) -> PathBuf {
    let path = expand_tilde(input);
    if path.is_absolute() {
        return path;
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(&path))
        .unwrap_or(path)
}

/// Request advisory file reservations on project-relative paths/globs.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `agent_name`: Agent requesting reservations
/// - `paths`: File paths or glob patterns (e.g., "app/api/*.py")
/// - `ttl_seconds`: Time to live (min 60s, default: 3600)
/// - `exclusive`: Exclusive intent (default: true)
/// - `reason`: Explanation for reservation
///
/// # Returns
/// Granted reservations and any conflicts
#[tool(description = "Request advisory file reservations on project-relative paths/globs.")]
pub async fn file_reservation_paths(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    paths: Vec<String>,
    ttl_seconds: Option<i64>,
    exclusive: Option<bool>,
    reason: Option<String>,
) -> McpResult<String> {
    if paths.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "paths list cannot be empty. Provide at least one file path or glob pattern to reserve (e.g., ['src/api/*.py', 'config/settings.yaml']).",
        ));
    }

    let ttl = ttl_seconds.unwrap_or(3600);
    if ttl < 60 {
        tracing::info!(
            "ttl_seconds={} is below recommended minimum (60s); continuing anyway",
            ttl
        );
    }

    let is_exclusive = exclusive.unwrap_or(true);
    let reason_str = reason.unwrap_or_default();

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent = resolve_agent(ctx, &pool, project_id, &agent_name).await?;
    let agent_id = agent.id.unwrap_or(0);

    // Check for conflicts with existing active reservations
    let active = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_active_reservations(ctx.cx(), &pool, project_id).await,
    )?;
    let agent_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;
    let agent_names: HashMap<i64, String> = agent_rows
        .into_iter()
        .filter_map(|row| row.id.map(|id| (id, row.name)))
        .collect();

    let mut conflicts: Vec<ReservationConflict> = Vec::new();
    let mut paths_to_grant: Vec<&str> = Vec::new();

    for path in &paths {
        // Check if any active exclusive reservation conflicts with this path
        let mut path_conflicts: Vec<ConflictHolder> = Vec::new();

        for res in &active {
            // Skip our own reservations
            if res.agent_id == agent_id {
                continue;
            }

            let is_conflict = res.exclusive != 0 && patterns_overlap(&res.path_pattern, path);

            if is_conflict {
                let agent_name = agent_names
                    .get(&res.agent_id)
                    .cloned()
                    .unwrap_or_else(|| format!("agent_{}", res.agent_id));
                path_conflicts.push(ConflictHolder {
                    agent_name,
                    reservation_id: res.id.unwrap_or(0),
                    expires_ts: micros_to_iso(res.expires_ts),
                });
            }
        }

        if path_conflicts.is_empty() {
            paths_to_grant.push(path);
        } else {
            conflicts.push(ReservationConflict {
                path: path.clone(),
                holders: path_conflicts,
            });
        }
    }

    // Grant non-conflicting reservations
    let granted_rows = if paths_to_grant.is_empty() {
        vec![]
    } else {
        db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::create_file_reservations(
                ctx.cx(),
                &pool,
                project_id,
                agent_id,
                &paths_to_grant,
                ttl,
                is_exclusive,
                &reason_str,
            )
            .await,
        )?
    };

    let granted: Vec<GrantedReservation> = granted_rows
        .iter()
        .map(|r| GrantedReservation {
            id: r.id.unwrap_or(0),
            path_pattern: r.path_pattern.clone(),
            exclusive: r.exclusive != 0,
            reason: r.reason.clone(),
            expires_ts: micros_to_iso(r.expires_ts),
        })
        .collect();

    // Write reservation artifacts to git archive (best-effort)
    if !granted_rows.is_empty() {
        let config = Config::from_env();
        match mcp_agent_mail_storage::ensure_archive(&config, &project.slug) {
            Ok(archive) => {
                let res_jsons: Vec<serde_json::Value> = granted_rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id.unwrap_or(0),
                            "agent": &agent_name,
                            "path_pattern": &r.path_pattern,
                            "exclusive": r.exclusive != 0,
                            "reason": &r.reason,
                            "expires_ts": micros_to_iso(r.expires_ts),
                        })
                    })
                    .collect();
                if let Err(e) = mcp_agent_mail_storage::write_file_reservation_records(
                    &archive, &config, &res_jsons,
                ) {
                    tracing::warn!("Failed to write reservation artifacts to archive: {e}");
                }
            }
            Err(e) => {
                tracing::warn!("Failed to ensure archive for reservation write: {e}");
            }
        }
    }

    let conflicts_len = conflicts.len();
    let response = ReservationResponse { granted, conflicts };

    tracing::debug!(
        "Reserved {} paths for {} in project {} (ttl: {}s, exclusive: {}, conflicts: {})",
        paths_to_grant.len(),
        agent_name,
        project_key,
        ttl,
        is_exclusive,
        conflicts_len
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Release active file reservations held by an agent.
///
/// If both paths and `file_reservation_ids` are omitted, releases all active reservations.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `agent_name`: Agent releasing reservations
/// - `paths`: Restrict release to matching path patterns
/// - `file_reservation_ids`: Restrict release to matching IDs
#[tool(description = "Release active file reservations held by an agent.")]
pub async fn release_file_reservations(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    paths: Option<Vec<String>>,
    file_reservation_ids: Option<Vec<i64>>,
) -> McpResult<String> {
    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent = resolve_agent(ctx, &pool, project_id, &agent_name).await?;
    let agent_id = agent.id.unwrap_or(0);

    // Convert paths to slice of &str
    let paths_ref: Option<Vec<&str>> = paths
        .as_ref()
        .map(|p| p.iter().map(String::as_str).collect());

    let released = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::release_reservations(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            paths_ref.as_deref(),
            file_reservation_ids.as_deref(),
        )
        .await,
    )?;

    let response = ReleaseResult {
        released: i32::try_from(released).unwrap_or(i32::MAX),
        released_at: micros_to_iso(mcp_agent_mail_db::now_micros()),
    };

    tracing::debug!(
        "Released {} reservations for {} in project {} (paths: {:?}, ids: {:?})",
        released,
        agent_name,
        project_key,
        paths,
        file_reservation_ids
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Extend expiry for active file reservations.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `agent_name`: Agent renewing reservations
/// - `extend_seconds`: Seconds to extend from max(now, expiry) (min 60s, default: 1800)
/// - `paths`: Restrict to matching path patterns
/// - `file_reservation_ids`: Restrict to matching IDs
#[tool(description = "Extend expiry for active file reservations.")]
pub async fn renew_file_reservations(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    extend_seconds: Option<i64>,
    paths: Option<Vec<String>>,
    file_reservation_ids: Option<Vec<i64>>,
) -> McpResult<String> {
    let extend = extend_seconds.unwrap_or(1800);

    // Validate extension >= 60 seconds
    if extend < 60 {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "extend_seconds must be at least 60 seconds",
        ));
    }

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent = resolve_agent(ctx, &pool, project_id, &agent_name).await?;
    let agent_id = agent.id.unwrap_or(0);

    // Convert paths to slice of &str
    let paths_ref: Option<Vec<&str>> = paths
        .as_ref()
        .map(|p| p.iter().map(String::as_str).collect());

    let renewed_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::renew_reservations(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            extend,
            paths_ref.as_deref(),
            file_reservation_ids.as_deref(),
        )
        .await,
    )?;

    let file_reservations: Vec<RenewedReservation> = renewed_rows
        .iter()
        .map(|r| {
            // Calculate old expiry (current - extend)
            let old_expires = r.expires_ts - (extend * 1_000_000);
            RenewedReservation {
                id: r.id.unwrap_or(0),
                path_pattern: r.path_pattern.clone(),
                old_expires_ts: micros_to_iso(old_expires),
                new_expires_ts: micros_to_iso(r.expires_ts),
            }
        })
        .collect();

    let response = RenewalResult {
        renewed: i32::try_from(file_reservations.len()).unwrap_or(i32::MAX),
        file_reservations,
    };

    tracing::debug!(
        "Renewed {} reservations for {} in project {} (+{}s, paths: {:?}, ids: {:?})",
        response.renewed,
        agent_name,
        project_key,
        extend,
        paths,
        file_reservation_ids
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Force-release a stale file reservation held by another agent.
///
/// Validates that the reservation appears abandoned (agent inactive beyond threshold
/// and no recent mail/filesystem/git activity).
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `agent_name`: Agent performing the force release
/// - `file_reservation_id`: ID of reservation to release
/// - `note`: Optional explanation
/// - `notify_previous`: Send notification to previous holder (default: true)
#[tool(description = "Force-release a stale file reservation after inactivity heuristics.")]
#[allow(clippy::too_many_lines)]
pub async fn force_release_file_reservation(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    file_reservation_id: i64,
    note: Option<String>,
    notify_previous: Option<bool>,
) -> McpResult<String> {
    let should_notify = notify_previous.unwrap_or(true);
    let now_iso = chrono::Utc::now().to_rfc3339();

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);
    let actor = resolve_agent(ctx, &pool, project_id, &agent_name).await?;

    let reservations = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_file_reservations(ctx.cx(), &pool, project_id, false)
            .await,
    )?;
    let reservation = reservations
        .into_iter()
        .find(|row| row.id.unwrap_or(0) == file_reservation_id);

    let Some(reservation) = reservation else {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            format!(
                "File reservation id={file_reservation_id} not found for project '{}'.",
                project.human_key
            ),
        ));
    };

    // If already released, return early
    if let Some(released_ts) = reservation.released_ts {
        let response = serde_json::json!({
            "released": 0,
            "released_at": micros_to_iso(released_ts),
            "already_released": true,
        });
        return serde_json::to_string(&response)
            .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")));
    }

    // Configurable thresholds
    let inactivity_seconds: i64 = 30 * 60; // 30 minutes
    let grace_seconds: i64 = 15 * 60; // 15 minutes
    let inactivity_micros = inactivity_seconds * 1_000_000;
    let grace_micros = grace_seconds * 1_000_000;

    // Validate inactivity heuristics (4 signals)
    let holder_agent = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_agent_by_id(ctx.cx(), &pool, reservation.agent_id).await,
    )?;

    let now_micros = mcp_agent_mail_db::now_micros();
    let mut stale_reasons = Vec::new();

    // Signal 1: Agent inactivity
    let agent_inactive_secs = (now_micros - holder_agent.last_active_ts) / 1_000_000;
    let agent_inactive = (now_micros - holder_agent.last_active_ts) > inactivity_micros;
    if agent_inactive {
        stale_reasons.push(format!("agent_inactive>{inactivity_seconds}s"));
    } else {
        stale_reasons.push("agent_recently_active".to_string());
    }

    // Signal 2: Mail activity
    let mail_activity = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_agent_last_mail_activity(
            ctx.cx(),
            &pool,
            reservation.agent_id,
            project_id,
        )
        .await,
    )?;
    let mail_stale = mail_activity.is_none_or(|ts| (now_micros - ts) > grace_micros);
    if mail_stale {
        stale_reasons.push(format!("no_recent_mail_activity>{grace_seconds}s"));
    } else {
        stale_reasons.push("mail_activity_recent".to_string());
    }

    // Signal 3: Git activity (via archive commits)
    let config = Config::from_env();
    let git_activity = get_git_activity_for_agent(&config, &project.slug, &holder_agent.name);
    let git_stale = git_activity.is_none_or(|ts| (now_micros - ts) > grace_micros);
    if git_stale {
        stale_reasons.push(format!("no_recent_git_activity>{grace_seconds}s"));
    } else {
        stale_reasons.push("git_activity_recent".to_string());
    }

    // Check if reservation has expired
    let is_expired = reservation.expires_ts < now_micros;

    // Must be inactive (agent + all signals stale) OR expired to force-release
    let all_signals_stale = agent_inactive && mail_stale && git_stale;
    if !all_signals_stale && !is_expired {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            format!(
                "Cannot force-release: heuristics do not indicate abandonment. Agent '{}' last active {}s ago. \
                 Signals: {}. Force release requires all signals stale or reservation expired.",
                holder_agent.name,
                agent_inactive_secs,
                stale_reasons.join(", ")
            ),
        ));
    }

    // Actually release the reservation in DB
    let released_count = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::force_release_reservation(ctx.cx(), &pool, file_reservation_id)
            .await,
    )?;

    // Optionally send notification to previous holder
    let notified = if should_notify && released_count > 0 && holder_agent.name != agent_name {
        let note_text = note.as_deref().unwrap_or("");
        let signals_md = stale_reasons
            .iter()
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n");

        let mut details = String::new();
        let _ = writeln!(
            details,
            "- last agent activity \u{2248} {agent_inactive_secs}s ago"
        );
        if let Some(ts) = mail_activity {
            let _ = writeln!(
                details,
                "- last mail activity \u{2248} {}s ago",
                (now_micros - ts) / 1_000_000
            );
        }
        if let Some(ts) = git_activity {
            let _ = writeln!(
                details,
                "- last git commit \u{2248} {}s ago",
                (now_micros - ts) / 1_000_000
            );
        }
        let _ = write!(
            details,
            "- inactivity threshold={inactivity_seconds}s grace={grace_seconds}s"
        );

        let notify_body = format!(
            "Your file reservation on `{}` (id={}) was force-released by **{}**.\n\n\
             **Observed signals:**\n{}\n\n\
             **Details:**\n{}\n\n\
             {}\n\n\
             You can re-acquire the reservation if still needed.",
            reservation.path_pattern,
            file_reservation_id,
            agent_name,
            signals_md,
            details,
            if note_text.is_empty() {
                String::new()
            } else {
                format!("**Note:** {note_text}")
            },
        );

        let result = mcp_agent_mail_db::queries::create_message(
            ctx.cx(),
            &pool,
            project_id,
            actor.id.unwrap_or(0),
            &format!(
                "[file-reservations] Released stale lock on {}",
                reservation.path_pattern
            ),
            &notify_body,
            None,
            "normal",
            false,
            "[]",
        )
        .await;
        matches!(result, asupersync::Outcome::Ok(_))
    } else {
        false
    };

    // Build response matching Python format
    let response = serde_json::json!({
        "released": released_count,
        "released_at": now_iso,
        "reservation": {
            "id": file_reservation_id,
            "agent": holder_agent.name,
            "path_pattern": reservation.path_pattern,
            "exclusive": reservation.exclusive != 0,
            "reason": reservation.reason,
            "created_ts": micros_to_iso(reservation.created_ts),
            "expires_ts": micros_to_iso(reservation.expires_ts),
            "released_ts": now_iso,
            "stale_reasons": stale_reasons,
            "last_agent_activity_ts": micros_to_iso(holder_agent.last_active_ts),
            "last_mail_activity_ts": mail_activity.map(micros_to_iso),
            "last_filesystem_activity_ts": serde_json::Value::Null,
            "last_git_activity_ts": git_activity.map(micros_to_iso),
            "notified": notified,
        },
    });

    tracing::debug!(
        "Force released reservation {} by {} in project {} (notify: {}, stale_reasons: {:?})",
        file_reservation_id,
        agent_name,
        project_key,
        should_notify,
        stale_reasons
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Get the most recent git activity timestamp for an agent (from archive commits).
fn get_git_activity_for_agent(
    config: &Config,
    project_slug: &str,
    agent_name: &str,
) -> Option<i64> {
    let archive = mcp_agent_mail_storage::ensure_archive(config, project_slug).ok()?;
    let commits = mcp_agent_mail_storage::get_commits_by_author(&archive, agent_name, 1).ok()?;
    commits.first().and_then(|c| {
        // Parse ISO-8601 date string to micros
        chrono::DateTime::parse_from_rfc3339(&c.date)
            .ok()
            .map(|dt| dt.timestamp_micros())
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(&c.date, "%Y-%m-%dT%H:%M:%S%.f")
                    .ok()
                    .map(|dt| dt.and_utc().timestamp_micros())
            })
    })
}

/// Install pre-commit guard for file reservation enforcement.
///
/// Creates a chain-runner hook and an Agent Mail guard plugin that checks
/// staged files against active file reservations before allowing commits.
///
/// # Parameters
/// - `project_key`: Project identifier (human key or slug)
/// - `code_repo_path`: Absolute path to the git repository
///
/// # Returns
/// `{"hook": "<path>"}` where path is the installed hook location,
/// or `{"hook": ""}` if worktrees/guard is not enabled.
#[tool(description = "Install pre-commit guard for file reservation enforcement.")]
pub fn install_precommit_guard(
    _ctx: &McpContext,
    project_key: String,
    code_repo_path: String,
) -> McpResult<String> {
    let config = Config::from_env();
    if !config.worktrees_enabled {
        return serde_json::to_string(&serde_json::json!({ "hook": "" }))
            .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")));
    }

    let repo_path = normalize_repo_path(&code_repo_path);

    if !repo_path.exists() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            format!("Repository path does not exist: {}", repo_path.display()),
        ));
    }

    // Install the guard via the guard crate
    mcp_agent_mail_guard::install_guard(&project_key, &repo_path).map_err(|e| {
        McpError::new(
            McpErrorCode::InternalError,
            format!("Failed to install guard: {e}"),
        )
    })?;

    // Resolve the actual hook path (honors core.hooksPath, worktrees, etc.)
    let hooks_dir = mcp_agent_mail_guard::resolve_hooks_dir(&repo_path).map_err(|e| {
        McpError::new(
            McpErrorCode::InternalError,
            format!("Failed to resolve hooks dir: {e}"),
        )
    })?;

    let hook_path = hooks_dir.join("pre-commit").display().to_string();
    let response = serde_json::json!({ "hook": hook_path });

    tracing::debug!(
        "Installed pre-commit guard for project {} at {}",
        project_key,
        code_repo_path
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Uninstall pre-commit guard from a repository.
///
/// Removes the guard plugin and chain-runner (if no other plugins remain).
/// Restores any previously preserved hooks.
///
/// # Parameters
/// - `code_repo_path`: Path to the code repository
///
/// # Returns
/// `{"removed": true}` if guard artifacts were removed, `{"removed": false}` otherwise.
#[tool(description = "Uninstall pre-commit guard from a repository.")]
pub fn uninstall_precommit_guard(_ctx: &McpContext, code_repo_path: String) -> McpResult<String> {
    let repo_path = normalize_repo_path(&code_repo_path);

    if !repo_path.exists() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            format!("Repository path does not exist: {}", repo_path.display()),
        ));
    }

    // Check if guard is installed before uninstalling
    let was_installed = guard_is_installed(&repo_path);

    // Uninstall via the guard crate
    mcp_agent_mail_guard::uninstall_guard(&repo_path).map_err(|e| {
        McpError::new(
            McpErrorCode::InternalError,
            format!("Failed to uninstall guard: {e}"),
        )
    })?;

    let response = serde_json::json!({ "removed": was_installed });

    tracing::debug!("Uninstalled pre-commit guard from {}", code_repo_path);

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Check if the guard is currently installed in a repo.
fn guard_is_installed(repo_path: &std::path::Path) -> bool {
    let Ok(hooks_dir) = mcp_agent_mail_guard::resolve_hooks_dir(repo_path) else {
        return false;
    };

    // Check for our plugin in hooks.d/pre-commit/
    let plugin = hooks_dir
        .join("hooks.d")
        .join("pre-commit")
        .join("50-agent-mail.py");
    if plugin.exists() {
        return true;
    }

    // Check for legacy single-file hook
    let hook = hooks_dir.join("pre-commit");
    if let Ok(content) = std::fs::read_to_string(hook) {
        if content.contains("mcp-agent-mail") {
            return true;
        }
    }

    false
}
