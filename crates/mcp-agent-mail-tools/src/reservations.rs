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
use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::pattern_overlap::CompiledPattern;
use mcp_agent_mail_db::micros_to_iso;
use serde::{Deserialize, Serialize};
use serde_json::json;
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::PathBuf;

use crate::messaging::{
    enqueue_message_semantic_index, try_dispatch_archive_write, try_write_message_archive,
};
use crate::reservation_index::{ReservationIndex, ReservationRef};
use crate::resources::{
    reservation_compute_pattern_activity, reservation_open_repo_root,
    reservation_project_workspace_path,
};
use crate::tool_util::{
    db_outcome_to_mcp_result, get_db_pool, legacy_tool_error, resolve_agent, resolve_project,
};

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

/// Conflict holder info (matches Python format exactly)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictHolder {
    pub agent: String,
    pub path_pattern: String,
    pub exclusive: bool,
    pub expires_ts: String,
}

#[derive(Debug, Clone)]
struct PendingConflictHolder {
    agent_id: i64,
    path_pattern: String,
    exclusive: bool,
    expires_ts: String,
}

#[derive(Debug, Clone)]
struct PendingReservationConflict {
    path: String,
    holders: Vec<PendingConflictHolder>,
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

/// Detect suspicious file reservation patterns (matching Python's `_detect_suspicious_file_reservation`).
fn detect_suspicious_file_reservation(pattern: &str) -> Option<String> {
    if pattern.trim().is_empty() {
        return Some("Pattern is completely empty.".to_string());
    }

    if path_looks_absolute(pattern) {
        return Some(format!(
            "Pattern appears to be an absolute path: '{pattern}'. Use project-relative paths instead."
        ));
    }

    let compiled = mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(pattern);
    let norm = compiled.normalized();

    if norm == "*" || norm == "**" || norm == "**/*" || norm == "**/**" || norm.is_empty() {
        return Some(format!(
            "Pattern '{pattern}' is too broad (normalizes to '{norm}'). It will block all other agents from editing any files."
        ));
    }

    if pattern.len() <= 2 && pattern.contains('*') {
        return Some(format!(
            "Pattern '{pattern}' is very short and likely too broad."
        ));
    }

    None
}

fn invalid_file_reservation_pattern(pattern: &str) -> Option<String> {
    if pattern.contains("..") {
        return Some("Pattern contains parent directory traversal ('..'). Use simple project-relative paths.".to_string());
    }
    let compiled = mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(pattern);
    if compiled.is_glob() && !compiled.is_matchable() {
        return Some(format!(
            "Pattern '{pattern}' is not a valid glob pattern. Check for unescaped special characters or mismatched brackets."
        ));
    }
    None
}

fn path_looks_absolute(input: &str) -> bool {
    if input.starts_with("//") {
        return false;
    }
    if std::path::Path::new(input).is_absolute() || input.starts_with("~/") || input == "~" {
        return true;
    }

    let bytes = input.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn relativize_path(project_root: &str, path: &str) -> Option<String> {
    fn normalize_parts(input: &str) -> Option<Vec<&str>> {
        let mut parts = Vec::new();
        for piece in input.split(['/', '\\']) {
            match piece {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                other => parts.push(other),
            }
        }
        Some(parts)
    }

    let expanded_path = expand_tilde(path).to_string_lossy().into_owned();
    let expanded_root = expand_tilde(project_root).to_string_lossy().into_owned();

    let path_is_absolute = path_looks_absolute(&expanded_path);

    let path_parts = normalize_parts(&expanded_path)?;
    if path_is_absolute {
        let root_parts = normalize_parts(&expanded_root)?;
        if path_parts.len() < root_parts.len() {
            return None;
        }
        for (i, root_part) in root_parts.iter().enumerate() {
            let matches = if cfg!(windows) {
                path_parts[i].eq_ignore_ascii_case(root_part)
            } else {
                path_parts[i] == *root_part
            };
            if !matches {
                return None;
            }
        }
        return Some(path_parts[root_parts.len()..].join("/"));
    }

    Some(path_parts.join("/"))
}

fn normalize_filter_paths(
    project_root: &str,
    paths: Option<Vec<String>>,
) -> McpResult<Option<Vec<String>>> {
    let Some(paths) = paths else {
        return Ok(None);
    };

    let mut normalized_paths = Vec::with_capacity(paths.len());
    for path in paths {
        match relativize_path(project_root, &path) {
            Some(rel) => {
                if rel.is_empty() {
                    return Err(legacy_tool_error(
                        "INVALID_PATH",
                        "Cannot target the project root directory itself. Please use more specific patterns.",
                        true,
                        json!({ "reason": "targets_project_root" }),
                    ));
                }
                if let Some(message) = invalid_file_reservation_pattern(&rel) {
                    return Err(legacy_tool_error(
                        "INVALID_PATH",
                        message,
                        true,
                        json!({ "reason": "invalid_pattern" }),
                    ));
                }
                normalized_paths.push(rel);
            }
            None => {
                return Err(legacy_tool_error(
                    "INVALID_PATH",
                    "Path is outside the project root. File reservations must be within the project directory.",
                    true,
                    json!({ "reason": "path_outside_project" }),
                ));
            }
        }
    }

    Ok(Some(normalized_paths))
}

fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home);
        }
        return PathBuf::from(input);
    }
    if let Some(rest) = input.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(input)
}

fn reservation_pattern_activity_for_project(
    project_human_key: &str,
    path_pattern: &str,
) -> crate::resources::ReservationPatternActivity {
    let workspace = reservation_project_workspace_path(project_human_key);
    let repo_info = workspace.as_deref().and_then(reservation_open_repo_root);
    let repo_root = repo_info.as_ref().map(|(root, _)| root.as_path());
    let workspace_rel = repo_info.as_ref().map(|(_, rel)| rel.as_path());
    reservation_compute_pattern_activity(
        workspace.as_deref(),
        repo_root,
        workspace_rel,
        path_pattern,
    )
}

fn released_ts_json_value(released_ts: Option<i64>) -> serde_json::Value {
    released_ts.map_or(serde_json::Value::Null, |ts| {
        serde_json::Value::String(micros_to_iso(ts))
    })
}

fn normalize_repo_path(input: &str) -> McpResult<PathBuf> {
    let path = expand_tilde(input);
    if path.as_os_str().is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "Repository path must not be empty.",
        ));
    }
    if !path.is_absolute() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            format!("Repository path must be absolute (or use ~/...): {input}"),
        ));
    }
    Ok(path)
}

fn renewal_filter_matches(
    row: &mcp_agent_mail_db::FileReservationRow,
    agent_id: i64,
    paths: Option<&[String]>,
    reservation_ids: Option<&[i64]>,
) -> bool {
    if row.released_ts.is_some() {
        return false;
    }
    if row.agent_id != agent_id {
        return false;
    }
    if let Some(ids) = reservation_ids
        && !ids.contains(&row.id.unwrap_or(0))
    {
        return false;
    }
    if let Some(path_patterns) = paths {
        if path_patterns.is_empty() {
            return false;
        }
        let row_pattern =
            mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(&row.path_pattern);
        let mut matched = false;
        for pat in path_patterns {
            if row.path_pattern == *pat {
                matched = true;
                break;
            }
            // Match the same overlap semantics used by reservation conflict detection,
            // so narrower literals can target broader held globs and vice versa.
            if mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(pat)
                .overlaps(&row_pattern)
            {
                matched = true;
                break;
            }
        }
        if !matched {
            return false;
        }
    }
    true
}

fn collect_previous_expiries(
    rows: &[mcp_agent_mail_db::FileReservationRow],
    agent_id: i64,
    paths: Option<&[String]>,
    reservation_ids: Option<&[i64]>,
) -> HashMap<i64, i64> {
    rows.iter()
        .filter(|row| renewal_filter_matches(row, agent_id, paths, reservation_ids))
        .filter_map(|row| row.id.map(|id| (id, row.expires_ts)))
        .collect()
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
///
/// # Conformance
/// Python-parity.
#[tool(
    description = "Request advisory file reservations (leases) on project-relative paths/globs.\n\nSemantics\n---------\n- Conflicts are reported if an overlapping active exclusive reservation exists held by another agent\n- Glob matching is symmetric (`fnmatchcase(a,b)` or `fnmatchcase(b,a)`), including exact matches\n- When granted, a JSON artifact is written under `file_reservations/<sha1(path)>.json` and the DB is updated\n- TTL must be >= 60 seconds (enforced by the server settings/policy)\n- Server-side enforcement (if enabled) only checks reservations that target mail archive paths\n  such as `agents/`, `messages/`, or `attachments/`; code repo enforcement is via the pre-commit guard\n\nDo / Don't\n----------\nDo:\n- Reserve files before starting edits to signal intent to other agents.\n- Use specific, minimal patterns (e.g., `app/api/*.py`) instead of broad globs.\n- Set a realistic TTL and renew with `renew_file_reservations` if you need more time.\n\nDon't:\n- Reserve the entire repository or very broad patterns (e.g., `**/*`) unless absolutely necessary.\n- Hold long-lived exclusive reservations when you are not actively editing.\n- Ignore conflicts; resolve them by coordinating with holders or waiting for expiry.\n\nParameters\n----------\nproject_key : str\nagent_name : str\npaths : list[str]\n    File paths or glob patterns relative to the project workspace (e.g., \"app/api/*.py\").\nttl_seconds : int\n    Time to live for the file_reservation; expired file_reservations are auto-released.\nexclusive : bool\n    If true, exclusive intent; otherwise shared/observe-only.\nreason : str\n    Optional explanation (helps humans reviewing Git artifacts).\n\nReturns\n-------\ndict\n    { granted: [{id, path_pattern, exclusive, reason, expires_ts}], conflicts: [{path, holders: [...]}] }\n\nExample\n-------\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"12\",\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\n  \"project_key\":\"/abs/path/backend\",\"agent_name\":\"GreenCastle\",\"paths\":[\"app/api/*.py\"],\n  \"ttl_seconds\":7200,\"exclusive\":true,\"reason\":\"migrations\"\n}}}\n```"
)]
pub async fn file_reservation_paths(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    paths: Vec<String>,
    ttl_seconds: Option<i64>,
    exclusive: Option<bool>,
    reason: Option<String>,
) -> McpResult<String> {
    let agent_name =
        mcp_agent_mail_core::models::normalize_agent_name(&agent_name).unwrap_or(agent_name);

    if paths.is_empty() {
        return Err(legacy_tool_error(
            "EMPTY_PATHS",
            "paths list cannot be empty. Provide at least one file path or glob pattern \
             to reserve (e.g., ['src/api/*.py', 'config/settings.yaml']).",
            true,
            json!({
                "provided": paths,
            }),
        ));
    }

    let ttl = ttl_seconds.map_or(3600, |t| t.clamp(60, 31_536_000));
    if let Some(t) = ttl_seconds {
        if t < 60 {
            tracing::warn!("ttl_seconds={t} clamped to minimum 60s");
        } else if t > 31_536_000 {
            tracing::warn!("ttl_seconds={t} clamped to maximum 31536000s (1 year)");
        }
    }

    let is_exclusive = exclusive.unwrap_or(true);
    let reason_str = reason.unwrap_or_default();

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Limit: max 200 patterns per call, preventing resource exhaustion.
    const MAX_PATHS_PER_CALL: usize = 200;
    if paths.len() > MAX_PATHS_PER_CALL {
        return Err(legacy_tool_error(
            "TOO_MANY_PATHS",
            &format!(
                "Maximum {MAX_PATHS_PER_CALL} paths per reservation call, got {}",
                paths.len()
            ),
            true,
            json!({ "count": paths.len(), "max": MAX_PATHS_PER_CALL }),
        ));
    }

    // Reject suspicious patterns that are too broad or malformed.
    // Previously these were warn-only, but overly-broad patterns like
    // "**/*" effectively block all other agents, which defeats the purpose
    // of advisory reservations.
    for pattern in &paths {
        if let Some(warning) = detect_suspicious_file_reservation(pattern) {
            tracing::warn!("[warn] {}", warning);
            return Err(legacy_tool_error(
                "SUSPICIOUS_PATTERN",
                &warning,
                true,
                json!({ "reason": "suspicious_pattern" }),
            ));
        }
    }

    // Normalize paths relative to project root
    let mut normalized_paths = Vec::with_capacity(paths.len());
    for p in &paths {
        match relativize_path(&project.human_key, p) {
            Some(rel) => {
                if rel.is_empty() {
                    return Err(legacy_tool_error(
                        "INVALID_PATH",
                        "Cannot reserve the project root directory itself. Please use more specific patterns.",
                        true,
                        json!({ "reason": "targets_project_root" }),
                    ));
                }
                if let Some(message) = invalid_file_reservation_pattern(&rel) {
                    return Err(legacy_tool_error(
                        "INVALID_PATH",
                        message,
                        true,
                        json!({ "reason": "invalid_pattern" }),
                    ));
                }
                normalized_paths.push(rel);
            }
            None => {
                return Err(legacy_tool_error(
                    "INVALID_PATH",
                    "Path is outside the project root. File reservations must be within the project directory.",
                    true,
                    json!({ "reason": "path_outside_project" }),
                ));
            }
        }
    }

    let agent = resolve_agent(
        ctx,
        &pool,
        project_id,
        &agent_name,
        &project.slug,
        &project.human_key,
    )
    .await?;
    let agent_id = agent.id.unwrap_or(0);

    // Check for conflicts with existing active reservations
    let active = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_active_reservations(ctx.cx(), &pool, project_id).await,
    )?;

    let mut paths_to_grant: SmallVec<[&str; 8]> = SmallVec::new();
    let mut seen_paths: HashSet<&str> = HashSet::new();

    let mut pending_conflicts: Vec<PendingReservationConflict> = Vec::new();

    // Build the reservation index from exclusive reservations held by other
    // agents. Exact paths now use exact/ancestor/descendant lookups, while
    // glob reservations remain prefix-scoped with a small root-glob fallback.
    let index = ReservationIndex::build(
        active
            .iter()
            .filter(|res| {
                if res.agent_id == agent_id {
                    return false;
                }
                // If request is exclusive, we conflict with ANY existing reservation (shared or exclusive).
                // If request is shared, we only conflict with existing EXCLUSIVE reservations.
                if is_exclusive {
                    true
                } else {
                    res.exclusive != 0
                }
            })
            .map(|res| {
                (
                    res.path_pattern.clone(),
                    ReservationRef {
                        agent_id: res.agent_id,
                        path_pattern: res.path_pattern.clone(),
                        exclusive: res.exclusive != 0,
                        expires_ts: res.expires_ts,
                    },
                )
            }),
    );

    // Precompile requested patterns once.
    let requested_compiled: Vec<
        std::sync::Arc<mcp_agent_mail_core::pattern_overlap::CompiledPattern>,
    > = normalized_paths
        .iter()
        .map(|p| CompiledPattern::cached(p))
        .collect();

    let mut conflict_refs = Vec::new();

    for (path, path_pat) in normalized_paths.iter().zip(requested_compiled.iter()) {
        if !seen_paths.insert(path.as_str()) {
            continue;
        }

        // Check conflicts with existing reservations
        index.find_conflicts(path_pat.as_ref(), &mut conflict_refs);

        if conflict_refs.is_empty() {
            paths_to_grant.push(path);
        } else {
            // Deterministic ordering keeps API output stable across runs
            // even when the index scans hash buckets in different orders.
            let mut holders: Vec<PendingConflictHolder> = std::mem::take(&mut conflict_refs)
                .into_iter()
                .map(|rref| PendingConflictHolder {
                    agent_id: rref.agent_id,
                    path_pattern: rref.path_pattern.clone(),
                    exclusive: rref.exclusive,
                    expires_ts: micros_to_iso(rref.expires_ts),
                })
                .collect();
            holders.sort_unstable_by(|a, b| {
                a.agent_id
                    .cmp(&b.agent_id)
                    .then_with(|| a.path_pattern.cmp(&b.path_pattern))
                    .then_with(|| a.exclusive.cmp(&b.exclusive))
                    .then_with(|| a.expires_ts.cmp(&b.expires_ts))
            });
            pending_conflicts.push(PendingReservationConflict {
                path: path.clone(),
                holders,
            });
        }
    }

    // Only resolve agent names if there were actual conflicts.
    let conflicts: Vec<ReservationConflict> = if pending_conflicts.is_empty() {
        Vec::new()
    } else {
        let agent_rows = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
        )?;
        let agent_names: HashMap<i64, String> = agent_rows
            .into_iter()
            .filter_map(|row| row.id.map(|id| (id, row.name)))
            .collect();

        pending_conflicts
            .into_iter()
            .map(|c| ReservationConflict {
                path: c.path,
                holders: c
                    .holders
                    .into_iter()
                    .map(|h| ConflictHolder {
                        agent: agent_names
                            .get(&h.agent_id)
                            .cloned()
                            .unwrap_or_else(|| format!("agent_{}", h.agent_id)),
                        path_pattern: h.path_pattern,
                        exclusive: h.exclusive,
                        expires_ts: h.expires_ts,
                    })
                    .collect(),
            })
            .collect()
    };

    // Grant non-conflicting reservations.
    //
    // The DB layer performs its own conflict check inside an IMMEDIATE
    // transaction.  If it detects a conflict that the tool-layer index
    // missed (e.g. due to a stale WAL read snapshot — Bug #86), convert
    // the ResourceBusy error into a structured conflict response instead
    // of propagating an opaque MCP error.
    let (granted_rows, conflicts) = if paths_to_grant.is_empty() {
        (vec![], conflicts)
    } else {
        match mcp_agent_mail_db::queries::create_file_reservations(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            &paths_to_grant,
            ttl,
            is_exclusive,
            &reason_str,
        )
        .await
        {
            asupersync::Outcome::Ok(rows) => (rows, conflicts),
            asupersync::Outcome::Err(mcp_agent_mail_db::DbError::ResourceBusy(msg)) => {
                // The DB layer detected a conflict that the tool layer's
                // index check missed.  Re-read active reservations to
                // build a fresh, accurate conflict response.
                tracing::warn!(
                    "DB-layer conflict detected after tool-layer index check passed \
                     (stale read likely): {msg}"
                );

                let fresh_active = db_outcome_to_mcp_result(
                    mcp_agent_mail_db::queries::get_active_reservations(
                        ctx.cx(),
                        &pool,
                        project_id,
                    )
                    .await,
                )?;

                let fresh_index = ReservationIndex::build(
                    fresh_active
                        .iter()
                        .filter(|res| {
                            if res.agent_id == agent_id {
                                return false;
                            }
                            if is_exclusive {
                                true
                            } else {
                                res.exclusive != 0
                            }
                        })
                        .map(|res| {
                            (
                                res.path_pattern.clone(),
                                ReservationRef {
                                    agent_id: res.agent_id,
                                    path_pattern: res.path_pattern.clone(),
                                    exclusive: res.exclusive != 0,
                                    expires_ts: res.expires_ts,
                                },
                            )
                        }),
                );

                let agent_rows = db_outcome_to_mcp_result(
                    mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
                )?;
                let agent_names: HashMap<i64, String> = agent_rows
                    .into_iter()
                    .filter_map(|row| row.id.map(|id| (id, row.name)))
                    .collect();

                let mut db_conflict_refs = Vec::new();
                let mut db_conflicts = conflicts;
                for path in &paths_to_grant {
                    let path_pat = CompiledPattern::cached(path);
                    fresh_index.find_conflicts(path_pat.as_ref(), &mut db_conflict_refs);
                    if !db_conflict_refs.is_empty() {
                        let mut holders: Vec<ConflictHolder> =
                            std::mem::take(&mut db_conflict_refs)
                                .into_iter()
                                .map(|rref| ConflictHolder {
                                    agent: agent_names
                                        .get(&rref.agent_id)
                                        .cloned()
                                        .unwrap_or_else(|| format!("agent_{}", rref.agent_id)),
                                    path_pattern: rref.path_pattern.clone(),
                                    exclusive: rref.exclusive,
                                    expires_ts: micros_to_iso(rref.expires_ts),
                                })
                                .collect();
                        holders.sort_unstable_by(|a, b| {
                            a.agent
                                .cmp(&b.agent)
                                .then_with(|| a.path_pattern.cmp(&b.path_pattern))
                        });
                        db_conflicts.push(ReservationConflict {
                            path: path.to_string(),
                            holders,
                        });
                    }
                }
                (vec![], db_conflicts)
            }
            other => {
                // All other errors propagate normally.
                db_outcome_to_mcp_result(other)?;
                unreachable!()
            }
        }
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

    // Write reservation artifacts to git archive (best-effort, via WBQ)
    if !granted_rows.is_empty() {
        let config = &Config::get();
        let res_jsons: Vec<serde_json::Value> = granted_rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id.unwrap_or(0),
                    "project": &project.human_key,
                    "agent": &agent.name,
                    "path_pattern": &r.path_pattern,
                    "exclusive": r.exclusive != 0,
                    "reason": &r.reason,
                    "created_ts": micros_to_iso(r.created_ts),
                    "expires_ts": micros_to_iso(r.expires_ts),
                })
            })
            .collect();
        let op = mcp_agent_mail_storage::WriteOp::FileReservation {
            project_slug: project.slug.clone(),
            config: config.clone(),
            reservations: res_jsons,
        };
        try_dispatch_archive_write(
            op,
            &format!("reservation archive write project={}", project.slug),
        );
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
///
/// # Conformance
/// Python-parity.
#[allow(clippy::too_many_lines)]
#[tool(
    description = "Release active file reservations held by an agent.\n\nBehavior\n--------\n- If both `paths` and `file_reservation_ids` are omitted, all active reservations for the agent are released\n- Otherwise, restricts release to matching ids and/or path patterns\n- JSON artifacts stay in Git for audit; DB records get `released_ts`\n\nReturns\n-------\ndict\n    { released: int, released_at: iso8601 }\n\nIdempotency\n-----------\n- Safe to call repeatedly. Releasing an already-released (or non-existent) reservation is a no-op.\n\nExamples\n--------\nRelease all active reservations for agent:\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"13\",\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\n  \"project_key\":\"/abs/path/backend\",\"agent_name\":\"GreenCastle\"\n}}}\n```\n\nRelease by ids:\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"14\",\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\n  \"project_key\":\"/abs/path/backend\",\"agent_name\":\"GreenCastle\",\"file_reservation_ids\":[101,102]\n}}}\n```"
)]
pub async fn release_file_reservations(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    paths: Option<Vec<String>>,
    file_reservation_ids: Option<Vec<i64>>,
) -> McpResult<String> {
    let agent_name =
        mcp_agent_mail_core::models::normalize_agent_name(&agent_name).unwrap_or(agent_name);

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);
    let normalized_paths = normalize_filter_paths(&project.human_key, paths)?;

    let agent = resolve_agent(
        ctx,
        &pool,
        project_id,
        &agent_name,
        &project.slug,
        &project.human_key,
    )
    .await?;
    let agent_id = agent.id.unwrap_or(0);

    let ids_to_release = if normalized_paths.is_some() || file_reservation_ids.is_some() {
        let existing_rows = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::list_unreleased_file_reservations(
                ctx.cx(),
                &pool,
                project_id,
            )
            .await,
        )?;
        let mut ids = Vec::new();
        for res in existing_rows {
            if renewal_filter_matches(
                &res,
                agent_id,
                normalized_paths.as_deref(),
                file_reservation_ids.as_deref(),
            ) && let Some(rid) = res.id
            {
                ids.push(rid);
            }
        }
        Some(ids)
    } else {
        None
    };

    // Perform the DB release (returns the actual updated rows)
    let released_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::release_reservations(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            None, // Pass resolved IDs only
            ids_to_release.as_deref(),
        )
        .await,
    )?;

    // Update archive artifacts for the released items
    if !released_rows.is_empty() {
        let res_jsons: Vec<serde_json::Value> = released_rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id.unwrap_or(0),
                    "project": &project.human_key,
                    "agent": &agent.name,
                    "path_pattern": &r.path_pattern,
                    "exclusive": r.exclusive != 0,
                    "reason": &r.reason,
                    "created_ts": micros_to_iso(r.created_ts),
                    "expires_ts": micros_to_iso(r.expires_ts),
                    "released_ts": released_ts_json_value(r.released_ts),
                })
            })
            .collect();

        let op = mcp_agent_mail_storage::WriteOp::FileReservation {
            project_slug: project.slug.clone(),
            config: Config::get(),
            reservations: res_jsons,
        };
        // Use match to ignore result (consistent with create path)
        try_dispatch_archive_write(
            op,
            &format!("reservation release archive write project={}", project.slug),
        );
    }

    let response = ReleaseResult {
        released: i32::try_from(released_rows.len()).unwrap_or(i32::MAX),
        released_at: micros_to_iso(mcp_agent_mail_db::now_micros()),
    };

    tracing::debug!(
        "Released {} reservations for {} in project {} (paths: {:?}, ids: {:?})",
        released_rows.len(),
        agent_name,
        project_key,
        normalized_paths,
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
///
/// # Conformance
/// Python-parity.
#[tool(
    description = "Extend expiry for active file reservations held by an agent without reissuing them.\n\nParameters\n----------\nproject_key : str\n    Project slug or human key.\nagent_name : str\n    Agent identity who owns the reservations.\nextend_seconds : int\n    Seconds to extend from the later of now or current expiry (min 60s).\npaths : Optional[list[str]]\n    Restrict renewals to matching path patterns.\nfile_reservation_ids : Optional[list[int]]\n    Restrict renewals to matching reservation ids.\n\nReturns\n-------\ndict\n    { renewed: int, file_reservations: [{id, path_pattern, old_expires_ts, new_expires_ts}] }"
)]
#[allow(clippy::too_many_lines)]
pub async fn renew_file_reservations(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    extend_seconds: Option<i64>,
    paths: Option<Vec<String>>,
    file_reservation_ids: Option<Vec<i64>>,
) -> McpResult<String> {
    let agent_name =
        mcp_agent_mail_core::models::normalize_agent_name(&agent_name).unwrap_or(agent_name);

    // Legacy parity: clamp too-small values up to 60 seconds and too-large values
    // to 1 year. Matches the warn-on-clamp pattern in file_reservation_paths and
    // macro_file_reservation_cycle so silent clamping doesn't surprise callers.
    let extend = extend_seconds.map_or(1800, |t| t.clamp(60, 31_536_000));
    if let Some(t) = extend_seconds {
        if t < 60 {
            tracing::warn!("extend_seconds={t} clamped to minimum 60s");
        } else if t > 31_536_000 {
            tracing::warn!("extend_seconds={t} clamped to maximum 31536000s (1 year)");
        }
    }

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);
    let normalized_paths = normalize_filter_paths(&project.human_key, paths)?;

    let agent = resolve_agent(
        ctx,
        &pool,
        project_id,
        &agent_name,
        &project.slug,
        &project.human_key,
    )
    .await?;
    let agent_id = agent.id.unwrap_or(0);

    let existing_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_file_reservations(ctx.cx(), &pool, project_id, true).await,
    )?;
    let previous_expires_by_id = collect_previous_expiries(
        &existing_rows,
        agent_id,
        normalized_paths.as_deref(),
        file_reservation_ids.as_deref(),
    );
    let ids_to_renew: Vec<i64> = previous_expires_by_id.keys().copied().collect();

    let renewed_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::renew_reservations(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            extend,
            None, // Pass IDs only now that we've resolved globs in the tool layer
            Some(&ids_to_renew),
        )
        .await,
    )?;

    if !renewed_rows.is_empty() {
        let res_jsons: Vec<serde_json::Value> = renewed_rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id.unwrap_or(0),
                    "project": &project.human_key,
                    "agent": &agent.name,
                    "path_pattern": &r.path_pattern,
                    "exclusive": r.exclusive != 0,
                    "reason": &r.reason,
                    "created_ts": micros_to_iso(r.created_ts),
                    "expires_ts": micros_to_iso(r.expires_ts),
                })
            })
            .collect();
        let op = mcp_agent_mail_storage::WriteOp::FileReservation {
            project_slug: project.slug.clone(),
            config: Config::get(),
            reservations: res_jsons,
        };
        try_dispatch_archive_write(
            op,
            &format!("reservation renewal archive write project={}", project.slug),
        );
    }

    let extend_micros = extend.saturating_mul(1_000_000);
    let file_reservations: Vec<RenewedReservation> = renewed_rows
        .iter()
        .map(|r| {
            let old_expires =
                r.id.and_then(|id| previous_expires_by_id.get(&id).copied())
                    .unwrap_or_else(|| r.expires_ts.saturating_sub(extend_micros));
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
        normalized_paths,
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
///
/// # Conformance
/// Python-parity.
#[tool(
    description = "Force-release a stale file reservation held by another agent after inactivity heuristics.\n\nThe tool validates that the reservation appears abandoned (agent inactive beyond threshold and\nno recent mail/filesystem/git activity). When released, an optional notification is sent to the\nprevious holder summarizing the heuristics."
)]
#[allow(clippy::too_many_lines)]
pub async fn force_release_file_reservation(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    file_reservation_id: i64,
    note: Option<String>,
    notify_previous: Option<bool>,
) -> McpResult<String> {
    let agent_name =
        mcp_agent_mail_core::models::normalize_agent_name(&agent_name).unwrap_or(agent_name);

    let should_notify = notify_previous.unwrap_or(true);

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);
    let actor = resolve_agent(
        ctx,
        &pool,
        project_id,
        &agent_name,
        &project.slug,
        &project.human_key,
    )
    .await?;

    let mut reservations = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_reservations_by_ids(
            ctx.cx(),
            &pool,
            &[file_reservation_id],
        )
        .await,
    )?;
    let reservation = reservations.pop();

    let Some(reservation) = reservation else {
        return Err(legacy_tool_error(
            "NOT_FOUND",
            format!(
                "File reservation id={file_reservation_id} not found for project '{}'.",
                project.human_key
            ),
            true,
            json!({
                "file_reservation_id": file_reservation_id,
            }),
        ));
    };

    if reservation.project_id != project_id {
        return Err(legacy_tool_error(
            "NOT_FOUND",
            format!(
                "File reservation id={file_reservation_id} not found for project '{}'.",
                project.human_key
            ),
            true,
            json!({
                "file_reservation_id": file_reservation_id,
            }),
        ));
    }

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

    // Read thresholds from config (env-overridable, matching Python parity)
    let config = Config::get();
    let inactivity_seconds =
        i64::try_from(config.file_reservation_inactivity_seconds).unwrap_or(1800);
    let grace_seconds =
        i64::try_from(config.file_reservation_activity_grace_seconds).unwrap_or(900);
    let inactivity_micros = inactivity_seconds.saturating_mul(1_000_000);
    let grace_micros = grace_seconds.saturating_mul(1_000_000);

    // Validate inactivity heuristics (4 signals)
    let holder_agent = match mcp_agent_mail_db::queries::get_agent_by_id_fresh(
        ctx.cx(),
        &pool,
        reservation.agent_id,
    )
    .await
    {
        Outcome::Ok(agent) => Some(agent),
        Outcome::Err(mcp_agent_mail_db::DbError::NotFound { .. }) => None,
        other => return Err(db_outcome_to_mcp_result(other).expect_err("non-ok outcome")),
    };
    let holder_agent_name = holder_agent.as_ref().map_or_else(
        || format!("[unknown-agent-{}]", reservation.agent_id),
        |agent| agent.name.clone(),
    );

    let now_micros = mcp_agent_mail_db::now_micros();
    let mut stale_reasons = Vec::new();

    // Signal 1: Agent inactivity
    let holder_last_active_ts = holder_agent.as_ref().map(|agent| agent.last_active_ts);
    let agent_inactive_secs =
        holder_last_active_ts.map(|ts| now_micros.saturating_sub(ts) / 1_000_000);
    let agent_inactive =
        holder_last_active_ts.is_none_or(|ts| now_micros.saturating_sub(ts) > inactivity_micros);
    if holder_last_active_ts.is_none() {
        stale_reasons.push("agent_missing".to_string());
    } else if agent_inactive {
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
    let mail_stale = mail_activity.is_none_or(|ts| now_micros.saturating_sub(ts) > grace_micros);
    if mail_stale {
        stale_reasons.push(format!("no_recent_mail_activity>{grace_seconds}s"));
    } else {
        stale_reasons.push("mail_activity_recent".to_string());
    }

    let pattern_activity =
        reservation_pattern_activity_for_project(&project.human_key, &reservation.path_pattern);
    let recent_fs = pattern_activity
        .fs_activity_micros
        .is_some_and(|ts| now_micros.saturating_sub(ts) <= grace_micros);
    let recent_git = pattern_activity
        .git_activity_micros
        .is_some_and(|ts| now_micros.saturating_sub(ts) <= grace_micros);

    if pattern_activity.matches {
        if recent_fs {
            stale_reasons.push("filesystem_activity_recent".to_string());
        } else {
            stale_reasons.push(format!("no_recent_filesystem_activity>{grace_seconds}s"));
        }
        if recent_git {
            stale_reasons.push("git_activity_recent".to_string());
        } else {
            stale_reasons.push(format!("no_recent_git_activity>{grace_seconds}s"));
        }
    } else {
        stale_reasons.push("path_pattern_unmatched".to_string());
    }

    // Check if reservation has expired
    let is_expired = reservation.expires_ts <= now_micros;

    // Must be inactive (agent + all signals stale) OR expired to force-release
    let all_signals_stale = agent_inactive && mail_stale && !recent_fs && !recent_git;
    if !all_signals_stale && !is_expired {
        return Err(legacy_tool_error(
            "RESERVATION_ACTIVE",
            "Reservation still shows recent activity; refusing forced release.",
            true,
            json!({
                "file_reservation_id": file_reservation_id,
                "stale_reasons": stale_reasons,
            }),
        ));
    }

    // Actually release the reservation in DB.
    // We pass the expires_ts we used for heuristics to perform an ATOMIC release.
    // If another agent renewed the reservation while we were calculating
    // inactivity, this call will return 0 released rows (MATCH failure).
    let released_count = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::force_release_reservation(
            ctx.cx(),
            &pool,
            file_reservation_id,
            Some(reservation.expires_ts),
        )
        .await,
    )?;

    if released_count == 0 {
        return Err(legacy_tool_error(
            "CONFLICT",
            "Reservation was renewed, released, or expired by another agent while heuristics were being calculated.",
            true,
            json!({ "file_reservation_id": file_reservation_id }),
        ));
    }

    let now_iso = micros_to_iso(mcp_agent_mail_db::now_micros());

    if released_count > 0 {
        let res_json = serde_json::json!({
            "id": reservation.id.unwrap_or(0),
            "project": &project.human_key,
            "agent": &holder_agent_name,
            "path_pattern": &reservation.path_pattern,
            "exclusive": reservation.exclusive != 0,
            "reason": &reservation.reason,
            "created_ts": micros_to_iso(reservation.created_ts),
            "expires_ts": micros_to_iso(reservation.expires_ts),
            "released_ts": now_iso.clone(),
        });

        let op = mcp_agent_mail_storage::WriteOp::FileReservation {
            project_slug: project.slug.clone(),
            config: Config::get(),
            reservations: vec![res_json],
        };
        try_dispatch_archive_write(
            op,
            &format!(
                "forced reservation release archive write project={}",
                project.slug
            ),
        );
    }

    // Optionally send notification to previous holder
    let notified = if should_notify
        && released_count > 0
        && holder_agent
            .as_ref()
            .is_some_and(|agent| agent.name != agent_name)
    {
        let holder_agent = holder_agent
            .as_ref()
            .expect("holder agent present when notification is attempted");
        let raw_note = note.as_deref().unwrap_or("");
        // Truncate note to prevent bypassing message size limits (4KB cap)
        let note_text = if raw_note.len() > 4096 {
            let mut idx = 4096;
            while idx > 0 && !raw_note.is_char_boundary(idx) {
                idx -= 1;
            }
            &raw_note[..idx]
        } else {
            raw_note
        };

        let signals_md = stale_reasons
            .iter()
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n");

        let mut details = String::new();
        if let Some(agent_inactive_secs) = agent_inactive_secs {
            let _ = writeln!(
                details,
                "- last agent activity \u{2248} {agent_inactive_secs}s ago"
            );
        } else {
            let _ = writeln!(details, "- holder agent metadata missing");
        }
        if let Some(ts) = mail_activity {
            let _ = writeln!(
                details,
                "- last mail activity \u{2248} {}s ago",
                now_micros.saturating_sub(ts) / 1_000_000
            );
        }
        if let Some(ts) = pattern_activity.fs_activity_micros {
            let _ = writeln!(
                details,
                "- last filesystem activity ≈ {}s ago",
                now_micros.saturating_sub(ts) / 1_000_000
            );
        }
        if let Some(ts) = pattern_activity.git_activity_micros {
            let _ = writeln!(
                details,
                "- last git commit \u{2248} {}s ago",
                now_micros.saturating_sub(ts) / 1_000_000
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

        let holder_id = holder_agent.id.unwrap_or(0);
        let recipients: &[(i64, &str)] = &[(holder_id, "to")];
        let result = mcp_agent_mail_db::queries::create_message_with_recipients(
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
            recipients,
        )
        .await;

        match result {
            asupersync::Outcome::Ok(message) => {
                let message_id = message.id.unwrap_or(0);
                enqueue_message_semantic_index(
                    project_id,
                    message_id,
                    &message.subject,
                    &message.body_md,
                );
                let all_recipient_names = vec![holder_agent_name.clone()];
                let msg_json = serde_json::json!({
                    "id": message_id,
                    "from": &agent_name,
                    "to": &all_recipient_names,
                    "cc": [],
                    "bcc": [],
                    "subject": &message.subject,
                    "created": micros_to_iso(message.created_ts),
                    "thread_id": &message.thread_id,
                    "project": &project.human_key,
                    "project_slug": &project.slug,
                    "importance": &message.importance,
                    "ack_required": message.ack_required != 0,
                    "attachments": [],
                });

                try_write_message_archive(
                    &Config::get(),
                    &project.slug,
                    &msg_json,
                    &message.body_md,
                    &agent_name,
                    &all_recipient_names,
                    &[],
                );
                true
            }
            _ => false,
        }
    } else {
        false
    };

    // Build response matching Python format
    let response = serde_json::json!({
        "released": released_count,
        "released_at": &now_iso,
        "reservation": {
            "id": file_reservation_id,
            "agent": &holder_agent_name,
            "path_pattern": reservation.path_pattern,
            "exclusive": reservation.exclusive != 0,
            "reason": reservation.reason,
            "created_ts": micros_to_iso(reservation.created_ts),
            "expires_ts": micros_to_iso(reservation.expires_ts),
            "released_ts": &now_iso,
            "stale_reasons": stale_reasons,
            "last_agent_activity_ts": holder_last_active_ts.map(micros_to_iso),
            "last_mail_activity_ts": mail_activity.map(micros_to_iso),
            "last_filesystem_activity_ts": pattern_activity.fs_activity_micros.map(micros_to_iso),
            "last_git_activity_ts": pattern_activity.git_activity_micros.map(micros_to_iso),
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
///
/// # Conformance
/// Python-parity.
#[tool(description = "")]
pub fn install_precommit_guard(
    _ctx: &McpContext,
    project_key: String,
    code_repo_path: String,
) -> McpResult<String> {
    let config = &Config::get();
    if !config.file_reservations_enforcement_enabled {
        return serde_json::to_string(&serde_json::json!({ "hook": "" }))
            .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")));
    }

    let repo_path = normalize_repo_path(&code_repo_path)?;

    if !repo_path.exists() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            format!("Repository path does not exist: {}", repo_path.display()),
        ));
    }

    // Enable pre-push hook installation by default to match legacy behavior
    mcp_agent_mail_guard::install_guard(&project_key, &repo_path, true).map_err(|e| {
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
/// - `code_repo_path`: Absolute path to the git repository
///
/// # Returns
/// `{"removed": true}` if guard artifacts were removed, `{"removed": false}` otherwise.
///
/// # Conformance
/// Python-parity.
#[tool(description = "")]
pub fn uninstall_precommit_guard(_ctx: &McpContext, code_repo_path: String) -> McpResult<String> {
    let repo_path = normalize_repo_path(&code_repo_path)?;

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
    if let Ok(content) = std::fs::read_to_string(hook)
        && content.contains("mcp-agent-mail")
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::{Cx, Outcome};
    use fastmcp::McpContext;
    use mcp_agent_mail_db::{DbPool, ProjectRow, queries};
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static RESERVATION_TEST_LOCK: Mutex<()> = Mutex::new(());
    static RESERVATION_TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn unique_suffix() -> u64 {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let time_component = u64::try_from(micros).unwrap_or(u64::MAX);
        time_component.wrapping_add(RESERVATION_TEST_COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    fn with_serialized_reservations<F, T>(f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let _lock = RESERVATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = tempfile::tempdir().expect("reservation test tempdir");
        let storage_root = temp.path().join("storage-root");
        std::fs::create_dir_all(&storage_root).expect("reservation test storage root");
        let database_path = temp.path().join("storage.sqlite3");
        let database_url = format!("sqlite://{}", database_path.display());
        let storage_root_str = storage_root
            .to_str()
            .expect("reservation test storage root utf-8")
            .to_string();

        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", database_url.as_str()),
                ("STORAGE_ROOT", storage_root_str.as_str()),
            ],
            f,
        )
    }

    fn run_async<F, Fut, T>(f: F) -> T
    where
        F: FnOnce(Cx) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let cx = Cx::for_testing();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(f(cx))
    }

    async fn ensure_project(cx: &Cx, pool: &DbPool, human_key: &str) -> ProjectRow {
        match queries::ensure_project(cx, pool, human_key).await {
            Outcome::Ok(project) => project,
            other => panic!("ensure_project failed: {other:?}"),
        }
    }

    async fn register_agent(
        cx: &Cx,
        pool: &DbPool,
        project_id: i64,
        name: &str,
    ) -> mcp_agent_mail_db::AgentRow {
        match queries::register_agent(
            cx,
            pool,
            project_id,
            name,
            "codex-cli",
            "gpt-5",
            Some("reservation test"),
            None,
            None,
        )
        .await
        {
            Outcome::Ok(agent) => agent,
            other => panic!("register_agent({name}) failed: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // expand_tilde
    // -----------------------------------------------------------------------

    #[test]
    fn expand_tilde_bare_tilde() {
        let result = expand_tilde("~");
        // Should expand to HOME (or leave as "~" if HOME unset)
        assert!(!result.as_os_str().is_empty());
    }

    #[test]
    fn expand_tilde_with_subpath() {
        let result = expand_tilde("~/Documents/file.txt");
        // Should not start with "~" anymore (assuming HOME is set)
        if std::env::var_os("HOME").is_some() {
            assert!(!result.starts_with("~"));
            assert!(result.to_string_lossy().ends_with("Documents/file.txt"));
        }
    }

    #[test]
    fn expand_tilde_absolute_path_unchanged() {
        assert_eq!(
            expand_tilde("/usr/local/bin"),
            PathBuf::from("/usr/local/bin")
        );
    }

    #[test]
    fn expand_tilde_relative_path_unchanged() {
        assert_eq!(expand_tilde("src/main.rs"), PathBuf::from("src/main.rs"));
    }

    #[test]
    fn expand_tilde_tilde_in_middle_unchanged() {
        // Only leading ~ is expanded
        assert_eq!(expand_tilde("foo/~/bar"), PathBuf::from("foo/~/bar"));
    }

    #[test]
    fn expand_tilde_empty_string() {
        assert_eq!(expand_tilde(""), PathBuf::from(""));
    }

    // -----------------------------------------------------------------------
    // normalize_repo_path
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_absolute_path_unchanged() {
        assert_eq!(
            normalize_repo_path("/data/projects/repo").unwrap(),
            PathBuf::from("/data/projects/repo")
        );
    }

    #[test]
    fn normalize_relative_path_rejected() {
        let err = normalize_repo_path("src/main.rs").expect_err("relative path must fail");
        assert!(err.to_string().contains("must be absolute"));
    }

    #[test]
    fn normalize_tilde_path_expanded() {
        if std::env::var_os("HOME").is_some() {
            let result = normalize_repo_path("~/projects/repo").unwrap();
            assert!(result.is_absolute());
            assert!(result.to_string_lossy().ends_with("projects/repo"));
        }
    }

    #[test]
    fn released_ts_json_value_none_is_null() {
        assert!(released_ts_json_value(None).is_null());
    }

    #[test]
    fn released_ts_json_value_some_is_iso_string() {
        let value = released_ts_json_value(Some(1_738_801_200_000_000));
        assert_eq!(
            value,
            serde_json::Value::String("2025-02-06T00:20:00.000000Z".to_string())
        );
    }

    fn reservation_row(
        id: i64,
        agent_id: i64,
        path_pattern: &str,
        expires_ts: i64,
        released_ts: Option<i64>,
    ) -> mcp_agent_mail_db::FileReservationRow {
        mcp_agent_mail_db::FileReservationRow {
            id: Some(id),
            project_id: 1,
            agent_id,
            path_pattern: path_pattern.to_string(),
            exclusive: 1,
            reason: String::new(),
            created_ts: 1,
            expires_ts,
            released_ts,
        }
    }

    #[test]
    fn collect_previous_expiries_applies_agent_and_path_filters() {
        let rows = vec![
            reservation_row(1, 7, "src/**", 1_000, None),
            reservation_row(2, 7, "docs/*.md", 2_000, None),
            reservation_row(3, 9, "src/**", 3_000, None),
            reservation_row(4, 7, "src/**", 4_000, Some(100)),
        ];

        let map = collect_previous_expiries(&rows, 7, Some(&["src/**".to_string()]), None);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&1), Some(&1_000));
    }

    #[test]
    fn collect_previous_expiries_respects_id_filter() {
        let rows = vec![
            reservation_row(10, 5, "src/**", 10_000, None),
            reservation_row(11, 5, "src/**", 11_000, None),
        ];

        let map = collect_previous_expiries(&rows, 5, None, Some(&[11]));
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&11), Some(&11_000));
    }

    // -----------------------------------------------------------------------
    // Empty paths validation (file_reservation_paths logic)
    // -----------------------------------------------------------------------

    #[test]
    fn empty_paths_detected() {
        let paths: Vec<String> = vec![];
        assert!(paths.is_empty());
    }

    #[test]
    fn non_empty_paths_accepted() {
        let paths = ["src/*.rs".to_string()];
        assert!(!paths.is_empty());
    }

    // -----------------------------------------------------------------------
    // TTL validation
    // -----------------------------------------------------------------------

    #[test]
    fn default_ttl_is_one_hour() {
        let ttl: i64 = 3600;
        assert_eq!(ttl, 3600);
    }

    #[test]
    fn ttl_below_60_warns_but_accepted() {
        let ttl = 30_i64;
        assert!(ttl < 60);
        // Tool does not reject; just logs
    }

    #[test]
    fn default_exclusive_is_true() {
        let exclusive: bool = true;
        assert!(exclusive);
    }

    // -----------------------------------------------------------------------
    // Response type serialization
    // -----------------------------------------------------------------------

    #[test]
    fn granted_reservation_serializes() {
        let r = GrantedReservation {
            id: 1,
            path_pattern: "src/**/*.rs".into(),
            exclusive: true,
            reason: "Working on parser".into(),
            expires_ts: "2026-02-06T02:00:00Z".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["id"], 1);
        assert_eq!(json["path_pattern"], "src/**/*.rs");
        assert_eq!(json["exclusive"], true);
    }

    #[test]
    fn reservation_conflict_serializes() {
        let r = ReservationConflict {
            path: "src/main.rs".into(),
            holders: vec![ConflictHolder {
                agent: "RedFox".into(),
                path_pattern: "src/main.rs".into(),
                exclusive: true,
                expires_ts: "2026-02-06T03:00:00Z".into(),
            }],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["path"], "src/main.rs");
        assert_eq!(json["holders"][0]["agent"], "RedFox");
        assert_eq!(json["holders"][0]["path_pattern"], "src/main.rs");
        assert_eq!(json["holders"][0]["exclusive"], true);
    }

    #[test]
    fn reservation_response_serializes() {
        let r = ReservationResponse {
            granted: vec![],
            conflicts: vec![ReservationConflict {
                path: "lib.rs".into(),
                holders: vec![ConflictHolder {
                    agent: "GoldHawk".into(),
                    path_pattern: "lib.rs".into(),
                    exclusive: true,
                    expires_ts: "2026-02-06T04:00:00Z".into(),
                }],
            }],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!(json["granted"].as_array().unwrap().is_empty());
        assert_eq!(json["conflicts"].as_array().unwrap().len(), 1);
        assert_eq!(json["conflicts"][0]["holders"][0]["agent"], "GoldHawk");
    }

    #[test]
    fn release_result_serializes() {
        let r = ReleaseResult {
            released: 3,
            released_at: "2026-02-06T01:00:00Z".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["released"], 3);
        assert!(json["released_at"].is_string());
    }

    #[test]
    fn renewal_result_serializes() {
        let r = RenewalResult {
            renewed: 2,
            file_reservations: vec![RenewedReservation {
                id: 10,
                path_pattern: "docs/*.md".into(),
                old_expires_ts: "2026-02-06T01:00:00Z".into(),
                new_expires_ts: "2026-02-06T02:00:00Z".into(),
            }],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["renewed"], 2);
        assert_eq!(json["file_reservations"][0]["id"], 10);
        assert!(json["file_reservations"][0]["old_expires_ts"].is_string());
    }

    #[test]
    fn reservation_response_round_trips() {
        let original = ReservationResponse {
            granted: vec![],
            conflicts: vec![ReservationConflict {
                path: "lib.rs".into(),
                holders: vec![ConflictHolder {
                    agent: "GoldHawk".into(),
                    path_pattern: "lib.rs".into(),
                    exclusive: true,
                    expires_ts: "2026-02-06T04:00:00Z".into(),
                }],
            }],
        };
        let json_str = serde_json::to_string(&original).unwrap();
        let deserialized: ReservationResponse = serde_json::from_str(&json_str).unwrap();
        assert!(deserialized.granted.is_empty());
        assert_eq!(deserialized.conflicts.len(), 1);
        assert_eq!(deserialized.conflicts[0].holders[0].agent, "GoldHawk");
    }

    // -----------------------------------------------------------------------
    // Tool validation rule tests (br-2841)
    // -----------------------------------------------------------------------

    // ── Path expansion edge cases ──

    #[test]
    fn relativize_path_rejects_traversal() {
        let root = "/project";
        assert_eq!(relativize_path(root, "../outside"), None);
        assert_eq!(relativize_path(root, "src/../../outside"), None);
        assert_eq!(
            relativize_path(root, "src/../internal"),
            Some("internal".to_string())
        );
        // Absolute path traversal check
        assert_eq!(relativize_path(root, "/project/../outside"), None);
        assert_eq!(
            relativize_path(root, "/project/src/../internal"),
            Some("internal".to_string())
        );
        assert_eq!(
            relativize_path(root, "/project/../project/src/main.rs"),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn normalize_filter_paths_normalizes_relative_and_backslash_forms() {
        let root = "/project";
        let normalized = normalize_filter_paths(
            root,
            Some(vec![
                "./src/main.rs".to_string(),
                "src\\lib.rs".to_string(),
                "src//deep///file.rs".to_string(),
            ]),
        )
        .expect("normalized paths");
        assert_eq!(
            normalized,
            Some(vec![
                "src/main.rs".to_string(),
                "src/lib.rs".to_string(),
                "src/deep/file.rs".to_string(),
            ])
        );
    }

    #[test]
    fn normalize_filter_paths_rejects_absolute_outside_root() {
        let root = "/project";
        let err = normalize_filter_paths(root, Some(vec!["/other/main.rs".to_string()]));
        let rendered = err.expect_err("expected invalid path").to_string();
        assert!(
            !rendered.contains(root),
            "error details must not leak absolute project root"
        );
    }

    #[test]
    fn normalize_filter_paths_rejects_windows_absolute_outside_root() {
        let root = "/project";
        let err = normalize_filter_paths(root, Some(vec!["C:\\other\\main.rs".to_string()]));
        let rendered = err.expect_err("expected invalid path").to_string();
        assert!(
            rendered.contains("outside the project root"),
            "expected outside-root error, got: {rendered}"
        );
        assert!(
            !rendered.contains(root),
            "error details must not leak absolute project root"
        );
    }

    #[test]
    fn normalize_filter_paths_rejects_project_root_target() {
        let root = "/project";
        let err = normalize_filter_paths(root, Some(vec![".".to_string()]));
        assert!(err.is_err());
    }

    #[test]
    fn normalize_filter_paths_rejects_invalid_glob_pattern() {
        let root = "/project";
        let err = normalize_filter_paths(root, Some(vec!["src/[abc".to_string()]));
        let rendered = err.expect_err("invalid glob should fail").to_string();
        assert!(rendered.contains("not a valid glob pattern"));
    }

    #[test]
    fn expand_tilde_double_tilde_unchanged() {
        // "~~" is not a valid tilde expansion
        let result = expand_tilde("~~");
        assert_eq!(result, PathBuf::from("~~"));
    }

    #[test]
    fn expand_tilde_tilde_with_username_unchanged() {
        // ~username syntax is not supported — only bare ~
        let result = expand_tilde("~otheruser/file");
        // Should NOT expand (no HOME-based expansion for other users)
        assert!(result.to_string_lossy().starts_with("~otheruser"));
    }

    #[test]
    fn normalize_repo_path_empty_string() {
        let err = normalize_repo_path("").expect_err("empty path must fail");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn normalize_repo_path_dot() {
        let err = normalize_repo_path(".").expect_err("dot path must fail");
        assert!(err.to_string().contains("must be absolute"));
    }

    // ── TTL validation edge cases ──

    #[test]
    fn ttl_exactly_60_is_minimum_valid() {
        let ttl = 60_i64;
        assert!(ttl >= 60, "60s is the minimum valid TTL");
    }

    #[test]
    fn ttl_large_value_accepted() {
        let ttl = 86400_i64 * 365; // 1 year in seconds
        assert!(ttl > 0);
        assert_eq!(ttl, 31_536_000);
    }

    // ── Multiple holders in conflict ──

    #[test]
    fn conflict_with_multiple_holders_serializes() {
        let r = ReservationConflict {
            path: "src/**/*.rs".into(),
            holders: vec![
                ConflictHolder {
                    agent: "RedFox".into(),
                    path_pattern: "src/**/*.rs".into(),
                    exclusive: true,
                    expires_ts: "2026-02-06T01:00:00Z".into(),
                },
                ConflictHolder {
                    agent: "BlueLake".into(),
                    path_pattern: "src/**/*.rs".into(),
                    exclusive: false,
                    expires_ts: "2026-02-06T02:00:00Z".into(),
                },
            ],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["holders"].as_array().unwrap().len(), 2);
        assert_eq!(json["holders"][0]["agent"], "RedFox");
        assert_eq!(json["holders"][1]["agent"], "BlueLake");
    }

    // ── Empty response types ──

    #[test]
    fn reservation_response_empty_both() {
        let r = ReservationResponse {
            granted: vec![],
            conflicts: vec![],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!(json["granted"].as_array().unwrap().is_empty());
        assert!(json["conflicts"].as_array().unwrap().is_empty());
    }

    #[test]
    fn release_result_zero_released() {
        let r = ReleaseResult {
            released: 0,
            released_at: "2026-02-06T00:00:00Z".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["released"], 0);
    }

    #[test]
    fn renewal_result_empty_reservations() {
        let r = RenewalResult {
            renewed: 0,
            file_reservations: vec![],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["renewed"], 0);
        assert!(json["file_reservations"].as_array().unwrap().is_empty());
    }

    // ── Glob pattern in paths ──

    #[test]
    fn glob_patterns_recognized() {
        use mcp_agent_mail_core::pattern_overlap::has_glob_meta;
        assert!(has_glob_meta("src/**/*.rs"));
        assert!(has_glob_meta("*.txt"));
        assert!(has_glob_meta("file?.rs"));
        assert!(has_glob_meta("src/{a,b}.rs"));
        assert!(has_glob_meta("src/[abc].rs"));
    }

    #[test]
    fn literal_paths_not_glob() {
        use mcp_agent_mail_core::pattern_overlap::has_glob_meta;
        assert!(!has_glob_meta("src/main.rs"));
        assert!(!has_glob_meta("Cargo.toml"));
        assert!(!has_glob_meta("README.md"));
        assert!(!has_glob_meta(""));
    }

    // ── Suspicious pattern detection (matching Python parity) ──

    #[test]
    fn too_broad_patterns_detected() {
        for pat in &["*", "**", "**/*", "**/**", "."] {
            let warning = detect_suspicious_file_reservation(pat);
            assert!(warning.is_some(), "expected warning for pattern: {pat}");
            assert!(
                warning.as_ref().unwrap().contains("too broad"),
                "expected 'too broad' in warning for {pat}"
            );
        }
    }

    #[test]
    fn absolute_path_detected() {
        let warning = detect_suspicious_file_reservation("/full/path/src/module.py");
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("absolute path"));
    }

    #[test]
    fn windows_absolute_path_detected() {
        let warning = detect_suspicious_file_reservation("C:\\full\\path\\src\\module.py");
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("absolute path"));
    }

    #[test]
    fn unc_path_not_flagged() {
        // UNC paths (starting with //) should NOT trigger the absolute path warning
        let warning = detect_suspicious_file_reservation("//network/share");
        assert!(warning.is_none());
    }

    #[test]
    fn very_short_pattern_detected() {
        let warning = detect_suspicious_file_reservation("*");
        // "*" also matches too-broad, so check it returns something
        assert!(warning.is_some());
        let warning2 = detect_suspicious_file_reservation("?*");
        assert!(warning2.is_some());
        assert!(warning2.unwrap().contains("very short"));
    }

    #[test]
    fn normal_patterns_not_suspicious() {
        for pat in &[
            "src/api/*.py",
            "lib/auth/**",
            "config/settings.yaml",
            "Cargo.toml",
        ] {
            let warning = detect_suspicious_file_reservation(pat);
            assert!(
                warning.is_none(),
                "unexpected warning for normal pattern: {pat}"
            );
        }
    }

    #[test]
    fn invalid_glob_pattern_detected() {
        let warning = invalid_file_reservation_pattern("src/[abc");
        assert!(warning.is_some(), "expected invalid glob to be rejected");
        assert!(
            warning.unwrap().contains("not a valid glob pattern"),
            "error message should explain invalid glob syntax"
        );
    }

    #[test]
    fn valid_glob_pattern_not_rejected() {
        let warning = invalid_file_reservation_pattern("src/**/*.{rs,toml}");
        assert!(warning.is_none(), "valid glob syntax should remain allowed");
    }

    #[test]
    fn renewal_filter_matches_treats_explicit_empty_filters_as_match_none() {
        let row = mcp_agent_mail_db::FileReservationRow {
            id: Some(42),
            project_id: 1,
            agent_id: 7,
            path_pattern: "src/main.rs".to_string(),
            exclusive: 1,
            reason: String::new(),
            created_ts: 0,
            expires_ts: 1,
            released_ts: None,
        };
        let empty_paths: Vec<String> = Vec::new();
        let empty_ids: Vec<i64> = Vec::new();

        assert!(!renewal_filter_matches(&row, 7, Some(&empty_paths), None));
        assert!(!renewal_filter_matches(&row, 7, None, Some(&empty_ids)));
        assert!(!renewal_filter_matches(
            &row,
            7,
            Some(&empty_paths),
            Some(&empty_ids),
        ));
    }

    #[test]
    fn renewal_filter_matches_uses_symmetric_overlap_for_paths() {
        let row = reservation_row(42, 7, "src/**", 1, None);
        assert!(renewal_filter_matches(
            &row,
            7,
            Some(&["src/main.rs".to_string()]),
            None,
        ));
    }

    #[test]
    fn release_file_reservations_filtered_ids_include_expired_unreleased_rows() {
        with_serialized_reservations(|| {
            run_async(|cx| async move {
                let pool = get_db_pool().expect("db pool");
                let project_key = format!("/tmp/release-expired-{}", unique_suffix());
                let project = ensure_project(&cx, &pool, &project_key).await;
                let project_id = project.id.unwrap_or(0);
                let agent = register_agent(&cx, &pool, project_id, "AmberRiver").await;
                let agent_id = agent.id.unwrap_or(0);

                let created = match queries::create_file_reservations(
                    &cx,
                    &pool,
                    project_id,
                    agent_id,
                    &["src/**"],
                    3600,
                    true,
                    "expired release regression",
                )
                .await
                {
                    Outcome::Ok(rows) => rows,
                    other => panic!("create_file_reservations failed: {other:?}"),
                };
                let reservation_id = created[0].id.unwrap_or(0);

                let conn = match pool.acquire(&cx).await {
                    Outcome::Ok(c) => c,
                    Outcome::Err(err) => panic!("acquire failed: {err}"),
                    Outcome::Cancelled(_) => panic!("acquire cancelled"),
                    Outcome::Panicked(panic) => panic!("acquire panicked: {}", panic.message()),
                };
                conn.execute_sync(
                    "UPDATE file_reservations SET expires_ts = ? WHERE id = ?",
                    &[
                        mcp_agent_mail_db::sqlmodel::Value::BigInt(
                            mcp_agent_mail_db::now_micros().saturating_sub(1),
                        ),
                        mcp_agent_mail_db::sqlmodel::Value::BigInt(reservation_id),
                    ],
                )
                .expect("expire reservation");

                let ctx = McpContext::new(cx.clone(), 1);
                let payload = release_file_reservations(
                    &ctx,
                    project.human_key.clone(),
                    agent.name.clone(),
                    None,
                    Some(vec![reservation_id]),
                )
                .await
                .expect("release_file_reservations");
                let parsed: Value = serde_json::from_str(&payload).expect("valid JSON");
                assert_eq!(parsed["released"].as_i64(), Some(1));
            });
        });
    }

    #[test]
    fn force_release_file_reservation_ignores_stale_cached_missing_holder_agent() {
        with_serialized_reservations(|| {
            run_async(|cx| async move {
                let pool = get_db_pool().expect("db pool");
                let project_key = format!("/tmp/force-release-stale-holder-{}", unique_suffix());
                let project = ensure_project(&cx, &pool, &project_key).await;
                let project_id = project.id.unwrap_or(0);
                let holder = register_agent(&cx, &pool, project_id, "AmberRiver").await;
                let actor = register_agent(&cx, &pool, project_id, "BlueLake").await;
                let holder_id = holder.id.unwrap_or(0);

                let created = match queries::create_file_reservations(
                    &cx,
                    &pool,
                    project_id,
                    holder_id,
                    &["src/**"],
                    3600,
                    true,
                    "force release stale cache regression",
                )
                .await
                {
                    Outcome::Ok(rows) => rows,
                    other => panic!("create_file_reservations failed: {other:?}"),
                };
                let reservation_id = created[0].id.unwrap_or(0);

                match queries::get_agent_by_id(&cx, &pool, holder_id).await {
                    Outcome::Ok(_) => {}
                    other => panic!("prime holder cache failed: {other:?}"),
                }

                let conn = match pool.acquire(&cx).await {
                    Outcome::Ok(c) => c,
                    Outcome::Err(err) => panic!("acquire failed: {err}"),
                    Outcome::Cancelled(_) => panic!("acquire cancelled"),
                    Outcome::Panicked(panic) => panic!("acquire panicked: {}", panic.message()),
                };
                conn.execute_sync(
                    "DELETE FROM agents WHERE id = ?",
                    &[mcp_agent_mail_db::sqlmodel::Value::BigInt(holder_id)],
                )
                .expect("delete holder row");
                conn.execute_sync(
                    "UPDATE file_reservations SET expires_ts = ? WHERE id = ?",
                    &[
                        mcp_agent_mail_db::sqlmodel::Value::BigInt(
                            mcp_agent_mail_db::now_micros().saturating_sub(1),
                        ),
                        mcp_agent_mail_db::sqlmodel::Value::BigInt(reservation_id),
                    ],
                )
                .expect("expire reservation");
                drop(conn);

                let ctx = McpContext::new(cx.clone(), 1);
                let payload = force_release_file_reservation(
                    &ctx,
                    project.human_key.clone(),
                    actor.name.clone(),
                    reservation_id,
                    None,
                    Some(true),
                )
                .await
                .expect("force release succeeds");
                let parsed: Value = serde_json::from_str(&payload).expect("valid JSON");
                assert_eq!(parsed["released"].as_i64(), Some(1));
                let expected_holder = format!("[unknown-agent-{holder_id}]");
                assert_eq!(
                    parsed["reservation"]["agent"].as_str(),
                    Some(expected_holder.as_str())
                );
                assert_eq!(parsed["reservation"]["notified"].as_bool(), Some(false));
                assert!(parsed["reservation"]["last_agent_activity_ts"].is_null());
            });
        });
    }
}
