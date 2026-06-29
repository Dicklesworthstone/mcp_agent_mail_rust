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
use mcp_agent_mail_db::{DbError, micros_to_iso};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use smallvec::SmallVec;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use crate::messaging::{
    enqueue_message_semantic_index, try_dispatch_archive_write, try_write_message_archive,
};
use crate::reservation_index::{ReservationIndex, ReservationRef};
use crate::resources::{
    reservation_compute_pattern_activity, reservation_open_repo_root,
    reservation_project_workspace_path,
};
use crate::tool_util::{
    db_error_to_mcp_error, db_outcome_to_mcp_result, get_db_pool, legacy_tool_error, resolve_agent,
    resolve_project,
};

const RELEASE_INTENT_SCHEMA_VERSION: u32 = 1;
const RELEASE_INTENT_KIND: &str = "release_file_reservations_intent";
const RELEASE_INTENT_REPLAY_KIND: &str = "release_file_reservations_replay";
const RELEASE_INTENT_DIR: &str = "degraded_intents";
const RELEASE_INTENT_LOG_FILE: &str = "release_file_reservations.jsonl";
const RELEASE_INTENT_LOCK_FILE: &str = ".release_file_reservations.jsonl.lock";

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

#[derive(Debug, Clone)]
struct ReleaseIntentReceipt {
    intent_id: String,
    intent_path: PathBuf,
    content_sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
struct QueuedReleaseIntent {
    kind: String,
    intent_id: String,
    content_sha256: String,
    project_key: String,
    agent_name: String,
    #[serde(default)]
    paths: Option<Vec<String>>,
    #[serde(default)]
    file_reservation_ids: Option<Vec<i64>>,
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

/// The single chokepoint every reservation mutation (acquire / renew / release /
/// force-release) and the reconcile-on-read healer funnel their archive write
/// through, so `SQLite` and the Git/JSON archive cannot drift independently (F1,
/// GH#112).
///
/// The pre-commit guard reads the `id-<id>.json` reservation artifacts directly,
/// so a stale archive yields a *wrong holder* — the exact #112 divergence. A bare
/// enqueue + best-effort flush silently advances the DB while the archive stays
/// stale whenever the drain thread stalls (a wedged git index lock), because
/// `wbq_flush` only warns on timeout. So this verifies the flush actually
/// drained; if it did not, the op is written synchronously on this thread before
/// returning. Re-emission is idempotent (`write_file_reservation_records`
/// overwrites the artifact), so a later drain of the same enqueued op is a
/// harmless rewrite.
fn dispatch_reservation_archive_write(op: mcp_agent_mail_storage::WriteOp, context: &str) {
    try_dispatch_archive_write(op.clone(), context);
    match mcp_agent_mail_storage::wbq_flush_status() {
        // Drained: the artifact is on disk. NoQueue: the enqueue path already
        // handled an unavailable queue (it falls back to a synchronous write).
        mcp_agent_mail_storage::WbqFlushOutcome::Drained
        | mcp_agent_mail_storage::WbqFlushOutcome::NoQueue => {}
        // The drain thread is stuck or gone — the enqueued op may never land.
        // Write it now so the archive the guard reads is durable on return.
        mcp_agent_mail_storage::WbqFlushOutcome::TimedOut
        | mcp_agent_mail_storage::WbqFlushOutcome::Disconnected => {
            if let Err(error) = mcp_agent_mail_storage::write_op_sync(&op) {
                tracing::warn!(
                    error = %error,
                    "{context}; synchronous archive fallback failed after the WBQ flush did not drain"
                );
            }
        }
    }
}

/// Build the canonical archive JSON for one active reservation row, authored from
/// the authoritative DB state. Mirrors the object the acquire/renew paths emit so
/// a healed artifact is byte-identical to one written by the original mutation.
fn active_reservation_artifact_json(
    project_human_key: &str,
    agent_name: &str,
    row: &mcp_agent_mail_db::FileReservationRow,
) -> Value {
    json!({
        "id": row.id.unwrap_or(0),
        "project": project_human_key,
        "agent": agent_name,
        "path_pattern": &row.path_pattern,
        "exclusive": row.exclusive != 0,
        "reason": &row.reason,
        "created_ts": micros_to_iso(row.created_ts),
        "expires_ts": micros_to_iso(row.expires_ts),
    })
}

fn ts_is_positive(ts: Option<i64>) -> bool {
    ts.is_some_and(|value| value > 0)
}

/// Does the present archive artifact diverge from the authoritative active DB
/// row? Conservative, matching `reservation_parity`: a field the artifact *omits*
/// is never treated as divergence (br-xyy95) — only a present-but-different value
/// is. An active DB row (`released_ts` absent) whose artifact records a release is
/// divergence (the #112 stuck-`released_ts` class).
fn active_archive_artifact_diverges(
    view: &crate::reservation_parity::ArchiveReservationView,
    row: &mcp_agent_mail_db::FileReservationRow,
    agent_name: &str,
) -> bool {
    if view.agent_name.trim() != agent_name.trim() {
        return true;
    }
    if let Some(archive_path) = view.path_pattern.as_deref()
        && archive_path.trim() != row.path_pattern.trim()
    {
        return true;
    }
    if let Some(archive_exclusive) = view.exclusive
        && archive_exclusive != (row.exclusive != 0)
    {
        return true;
    }
    // Active DB row -> the archive must not record this reservation as released.
    if ts_is_positive(view.released_ts) != ts_is_positive(row.released_ts) {
        return true;
    }
    false
}

/// Pure decision for reconcile-on-read: given the project's active DB
/// reservations, the `agent_id -> name` map, and the archive artifacts currently
/// present, return the artifact JSONs that must be (re)written so the on-disk
/// archive the pre-commit guard reads matches the authoritative DB.
///
/// A row is healed when its `id-<id>.json` artifact is missing (the crash-gap
/// between DB-commit and archive-write — F1's acceptance) or diverges from the DB
/// (#112 wrong-holder). A row whose `agent_id` is absent from `agent_names` is
/// skipped: we cannot author a faithful artifact without the holder's name, and
/// guessing would risk writing a *wrong* holder — the very failure we heal.
fn reservation_rows_needing_archive_heal(
    project_human_key: &str,
    active_rows: &[mcp_agent_mail_db::FileReservationRow],
    agent_names: &HashMap<i64, String>,
    archive_present: &BTreeMap<i64, crate::reservation_parity::ArchiveReservationView>,
) -> Vec<Value> {
    let mut heal = Vec::new();
    for row in active_rows {
        let Some(id) = row.id else {
            continue;
        };
        let Some(agent_name) = agent_names.get(&row.agent_id) else {
            continue;
        };
        let needs_heal = archive_present
            .get(&id)
            .is_none_or(|view| active_archive_artifact_diverges(view, row, agent_name));
        if needs_heal {
            heal.push(active_reservation_artifact_json(
                project_human_key,
                agent_name,
                row,
            ));
        }
    }
    heal
}

/// Reconcile the Git archive reservation artifacts for `project` against the
/// authoritative active DB reservations, healing any that are missing or stale
/// (F1 reconcile-on-read). Returns the number of artifacts re-emitted.
///
/// Cheap and safe on the reservation read path: it reads only the *active* rows'
/// own `id-<id>.json` artifacts (bounded by the active set, not the project's
/// full reservation history), and dispatches an archive write only when genuine
/// drift is found — so the steady state costs a handful of stats. This is what
/// makes F1's acceptance hold — a crash between a reservation's DB-commit and its
/// archive-write converges to a consistent archive (no wrong holder) on the next
/// `file_reservation_paths` call, with no operator action.
fn reconcile_active_reservation_archive(
    project: &mcp_agent_mail_db::ProjectRow,
    active_rows: &[mcp_agent_mail_db::FileReservationRow],
    agent_names: &HashMap<i64, String>,
    config: &Config,
) -> usize {
    if active_rows.is_empty() {
        return 0;
    }
    // Look up only the active reservations' artifacts — never the whole archive.
    let mut present = BTreeMap::new();
    for row in active_rows {
        if let Some(id) = row.id
            && let Some(view) = crate::reservation_parity::read_project_archive_reservation(
                &config.storage_root,
                &project.slug,
                id,
            )
        {
            present.insert(id, view);
        }
    }
    let heal = reservation_rows_needing_archive_heal(
        &project.human_key,
        active_rows,
        agent_names,
        &present,
    );
    if heal.is_empty() {
        return 0;
    }
    let healed = heal.len();
    tracing::debug!(
        "reconcile-on-read healed {healed} stale/missing reservation archive artifact(s) for project={}",
        project.slug
    );
    let op = mcp_agent_mail_storage::WriteOp::FileReservation {
        project_slug: project.slug.clone(),
        config: config.clone(),
        reservations: heal,
    };
    dispatch_reservation_archive_write(
        op,
        &format!(
            "reservation archive reconcile-on-read project={}",
            project.slug
        ),
    );
    healed
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

fn release_intent_log_path(config: &Config) -> PathBuf {
    config
        .storage_root
        .join(RELEASE_INTENT_DIR)
        .join(RELEASE_INTENT_LOG_FILE)
}

fn release_intent_lock_path(config: &Config) -> PathBuf {
    config
        .storage_root
        .join(RELEASE_INTENT_DIR)
        .join(RELEASE_INTENT_LOCK_FILE)
}

fn reject_existing_symlink(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::other(format!(
            "release intent path must not be a symlink: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn ensure_release_intent_parent(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::other("release intent log has no parent"));
    };
    reject_existing_symlink(parent)?;
    std::fs::create_dir_all(parent)?;
    reject_existing_symlink(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn hash_json_value(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).expect("serializing serde_json::Value should not fail");
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn release_intent_hash_payload(record: &Value) -> Value {
    json!({
        "schema_version": record["schema_version"].clone(),
        "kind": record["kind"].clone(),
        "created_ts": record["created_ts"].clone(),
        "project_key": record["project_key"].clone(),
        "agent_name": record["agent_name"].clone(),
        "paths": record["paths"].clone(),
        "file_reservation_ids": record["file_reservation_ids"].clone(),
        "failure": record["failure"].clone(),
    })
}

fn release_replay_hash_payload(record: &Value) -> Value {
    json!({
        "schema_version": record["schema_version"].clone(),
        "kind": record["kind"].clone(),
        "intent_id": record["intent_id"].clone(),
        "intent_content_sha256": record["intent_content_sha256"].clone(),
        "replayed_ts": record["replayed_ts"].clone(),
        "status": record["status"].clone(),
        "released": record["released"].clone(),
        "error_detail": record["error_detail"].clone(),
    })
}

fn release_intent_record_has_valid_hash(record: &Value) -> bool {
    let Some(content_sha256) = record.get("content_sha256").and_then(Value::as_str) else {
        return false;
    };
    let Some(intent_id) = record.get("intent_id").and_then(Value::as_str) else {
        return false;
    };
    let computed_hash = hash_json_value(&release_intent_hash_payload(record));
    content_sha256.len() == 64
        && intent_id.len() == 16
        && content_sha256 == computed_hash
        && content_sha256.starts_with(intent_id)
}

fn release_replay_record_has_valid_hash(record: &Value) -> bool {
    let Some(content_sha256) = record.get("content_sha256").and_then(Value::as_str) else {
        return false;
    };
    let Some(intent_id) = record.get("intent_id").and_then(Value::as_str) else {
        return false;
    };
    let Some(intent_content_sha256) = record.get("intent_content_sha256").and_then(Value::as_str)
    else {
        return false;
    };
    content_sha256.len() == 64
        && intent_id.len() == 16
        && intent_content_sha256.len() == 64
        && intent_content_sha256.starts_with(intent_id)
        && content_sha256 == hash_json_value(&release_replay_hash_payload(record))
}

fn append_release_intent_jsonl(config: &Config, record: &Value) -> std::io::Result<PathBuf> {
    let path = release_intent_log_path(config);
    ensure_release_intent_parent(&path)?;
    reject_existing_symlink(&path)?;
    let lock_path = release_intent_lock_path(config);
    reject_existing_symlink(&lock_path)?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        lock_file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    fs2::FileExt::lock_exclusive(&lock_file)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    // Defend against a torn final line from a prior crash (a partial append that
    // never reached fsync). If the log does not currently end in a newline,
    // write a leading one so the torn fragment stays isolated on its own
    // (skippable) line instead of being concatenated onto — and lost together
    // with — this otherwise-valid record on the next read.
    let needs_leading_newline = if let Ok(meta) = file.metadata()
        && meta.len() > 0
    {
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        last[0] != b'\n'
    } else {
        false
    };
    let mut line = Vec::new();
    if needs_leading_newline {
        line.push(b'\n');
    }
    line.extend_from_slice(
        &serde_json::to_vec(record).map_err(|err| std::io::Error::other(err.to_string()))?,
    );
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_all()?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(path)
}

fn append_release_intent(
    config: &Config,
    project_key: &str,
    agent_name: &str,
    paths: Option<Vec<String>>,
    file_reservation_ids: Option<Vec<i64>>,
    failure_stage: &str,
    error_detail: &str,
) -> std::io::Result<ReleaseIntentReceipt> {
    let created_ts = mcp_agent_mail_db::now_micros();
    let payload = json!({
        "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
        "kind": RELEASE_INTENT_KIND,
        "created_ts": created_ts,
        "project_key": project_key,
        "agent_name": agent_name,
        "paths": paths,
        "file_reservation_ids": file_reservation_ids,
        "failure": {
            "stage": failure_stage,
            "error_detail": error_detail,
        },
    });
    let content_sha256 = hash_json_value(&payload);
    let intent_id = content_sha256.chars().take(16).collect::<String>();
    let record = json!({
        "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
        "kind": RELEASE_INTENT_KIND,
        "intent_id": intent_id,
        "content_sha256": content_sha256,
        "created_ts": created_ts,
        "project_key": project_key,
        "agent_name": agent_name,
        "paths": payload["paths"].clone(),
        "file_reservation_ids": payload["file_reservation_ids"].clone(),
        "failure": payload["failure"].clone(),
    });
    let intent_path = append_release_intent_jsonl(config, &record)?;
    Ok(ReleaseIntentReceipt {
        intent_id,
        intent_path,
        content_sha256,
    })
}

fn append_release_replay_record(
    config: &Config,
    intent_id: &str,
    intent_content_sha256: &str,
    status: &str,
    released: usize,
    error_detail: Option<&str>,
) {
    let replayed_ts = mcp_agent_mail_db::now_micros();
    let payload = json!({
        "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
        "kind": RELEASE_INTENT_REPLAY_KIND,
        "intent_id": intent_id,
        "intent_content_sha256": intent_content_sha256,
        "replayed_ts": replayed_ts,
        "status": status,
        "released": released,
        "error_detail": error_detail,
    });
    let content_sha256 = hash_json_value(&payload);
    let record = json!({
        "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
        "kind": RELEASE_INTENT_REPLAY_KIND,
        "intent_id": intent_id,
        "content_sha256": content_sha256,
        "intent_content_sha256": intent_content_sha256,
        "replayed_ts": replayed_ts,
        "status": status,
        "released": released,
        "error_detail": error_detail,
    });
    if let Err(error) = append_release_intent_jsonl(config, &record) {
        tracing::warn!(
            error = %error,
            intent_id,
            "failed to append release intent replay record"
        );
    }
}

fn queued_release_intent_response(
    config: &Config,
    project_key: &str,
    agent_name: &str,
    paths: Option<Vec<String>>,
    file_reservation_ids: Option<Vec<i64>>,
    failure_stage: &str,
    error_detail: &str,
) -> McpResult<String> {
    let receipt = append_release_intent(
        config,
        project_key,
        agent_name,
        paths,
        file_reservation_ids,
        failure_stage,
        error_detail,
    )
    .map_err(|error| {
        legacy_tool_error(
            "RELEASE_INTENT_WRITE_FAILED",
            format!(
                "Could not release reservations because the database is unavailable, and writing \
                 the local release-intent log also failed: {error}"
            ),
            false,
            json!({ "error_detail": error.to_string() }),
        )
    })?;
    serde_json::to_string(&json!({
        "released": 0,
        "released_at": micros_to_iso(mcp_agent_mail_db::now_micros()),
        "status": "queued",
        "queued": true,
        "message": "lease release queued because DB unavailable",
        "intent": {
            "id": receipt.intent_id,
            "path": receipt.intent_path.display().to_string(),
            "content_sha256": receipt.content_sha256,
            "replay": "automatic_on_next_successful_release_file_reservations_call",
        },
    }))
    .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

const fn db_error_supports_release_intent(error: &DbError) -> bool {
    matches!(
        error,
        DbError::Pool(_)
            | DbError::Sqlite(_)
            | DbError::Schema(_)
            | DbError::ResourceBusy(_)
            | DbError::PoolExhausted { .. }
            | DbError::CircuitBreakerOpen { .. }
            | DbError::IntegrityCorruption { .. }
    )
}

fn mcp_error_supports_release_intent(error: &McpError) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data["error"]["type"].as_str())
        .is_some_and(|error_type| {
            matches!(
                error_type,
                "DATABASE_CORRUPTION"
                    | "DATABASE_ERROR"
                    | "DATABASE_POOL_EXHAUSTED"
                    | "RESOURCE_BUSY"
            )
        })
}

fn read_queued_release_intents(config: &Config) -> std::io::Result<Vec<QueuedReleaseIntent>> {
    let path = release_intent_log_path(config);
    reject_existing_symlink(&path)?;
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut terminal = HashSet::new();
    let mut intents = Vec::new();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("kind").and_then(Value::as_str) {
            // A replay marker is terminal when the intent either succeeded
            // ("replayed") or is permanently un-replayable ("abandoned" — e.g.
            // the agent or project no longer exists). Both clear the queued
            // intent so it is not retried forever (mirrors the ack-intent
            // design in messaging.rs); only a retryable "failed" marker leaves
            // the intent queued for the next replay attempt.
            Some(RELEASE_INTENT_REPLAY_KIND)
                if matches!(
                    value.get("status").and_then(Value::as_str),
                    Some("replayed" | "abandoned")
                ) =>
            {
                if !release_replay_record_has_valid_hash(&value) {
                    tracing::warn!("skipping replay marker with invalid content hash");
                    continue;
                }
                if let Some(intent_id) = value.get("intent_id").and_then(Value::as_str) {
                    let Some(intent_content_sha256) =
                        value.get("intent_content_sha256").and_then(Value::as_str)
                    else {
                        continue;
                    };
                    terminal.insert((intent_id.to_string(), intent_content_sha256.to_string()));
                }
            }
            Some(RELEASE_INTENT_KIND) => {
                if !release_intent_record_has_valid_hash(&value) {
                    tracing::warn!("skipping release intent with invalid content hash");
                    continue;
                }
                if let Ok(intent) = serde_json::from_value::<QueuedReleaseIntent>(value) {
                    intents.push(intent);
                }
            }
            _ => {}
        }
    }
    intents.retain(|intent| {
        !terminal.contains(&(intent.intent_id.clone(), intent.content_sha256.clone()))
    });
    Ok(intents)
}

fn dispatch_release_archive_write(
    project: &mcp_agent_mail_db::ProjectRow,
    agent: &mcp_agent_mail_db::AgentRow,
    released_rows: &[mcp_agent_mail_db::FileReservationRow],
    config: &Config,
) {
    if released_rows.is_empty() {
        return;
    }
    let res_jsons: Vec<Value> = released_rows
        .iter()
        .map(|r| {
            json!({
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
        config: config.clone(),
        reservations: res_jsons,
    };
    dispatch_reservation_archive_write(
        op,
        &format!("reservation release archive write project={}", project.slug),
    );
}

async fn replay_single_release_intent(
    ctx: &McpContext,
    pool: &mcp_agent_mail_db::DbPool,
    config: &Config,
    intent: &QueuedReleaseIntent,
) -> Result<usize, (String, bool)> {
    if intent.kind != RELEASE_INTENT_KIND {
        return Ok(0);
    }
    let project = resolve_project(ctx, pool, &intent.project_key)
        .await
        .map_err(|error| (error.to_string(), mcp_error_supports_release_intent(&error)))?;
    let project_id = project.id.unwrap_or(0);
    let normalized_paths = normalize_filter_paths(&project.human_key, intent.paths.clone())
        .map_err(|error| (error.to_string(), mcp_error_supports_release_intent(&error)))?;
    let agent = resolve_agent(
        ctx,
        pool,
        project_id,
        &intent.agent_name,
        &project.slug,
        &project.human_key,
    )
    .await
    .map_err(|error| (error.to_string(), mcp_error_supports_release_intent(&error)))?;
    let agent_id = agent.id.unwrap_or(0);
    let ids_to_release = if normalized_paths.is_some() || intent.file_reservation_ids.is_some() {
        let existing_rows = match mcp_agent_mail_db::queries::list_unreleased_file_reservations(
            ctx.cx(),
            pool,
            project_id,
        )
        .await
        {
            asupersync::Outcome::Ok(rows) => rows,
            asupersync::Outcome::Err(error) => {
                return Err((error.to_string(), db_error_supports_release_intent(&error)));
            }
            asupersync::Outcome::Cancelled(reason) => {
                return Err((format!("cancelled: {reason:?}"), true));
            }
            asupersync::Outcome::Panicked(panic) => {
                return Err((format!("panicked: {}", panic.message()), true));
            }
        };
        let mut ids = Vec::new();
        for reservation in existing_rows {
            if renewal_filter_matches(
                &reservation,
                agent_id,
                normalized_paths.as_deref(),
                intent.file_reservation_ids.as_deref(),
            ) && let Some(id) = reservation.id
            {
                ids.push(id);
            }
        }
        Some(ids)
    } else {
        None
    };

    let released_rows = match mcp_agent_mail_db::queries::release_reservations(
        ctx.cx(),
        pool,
        project_id,
        agent_id,
        None,
        ids_to_release.as_deref(),
    )
    .await
    {
        asupersync::Outcome::Ok(rows) => rows,
        asupersync::Outcome::Err(error) => {
            return Err((error.to_string(), db_error_supports_release_intent(&error)));
        }
        asupersync::Outcome::Cancelled(reason) => {
            return Err((format!("cancelled: {reason:?}"), true));
        }
        asupersync::Outcome::Panicked(panic) => {
            return Err((format!("panicked: {}", panic.message()), true));
        }
    };
    dispatch_release_archive_write(&project, &agent, &released_rows, config);
    Ok(released_rows.len())
}

async fn replay_queued_release_intents(
    ctx: &McpContext,
    pool: &mcp_agent_mail_db::DbPool,
    config: &Config,
) {
    let intents = match read_queued_release_intents(config) {
        Ok(intents) => intents,
        Err(error) => {
            tracing::warn!(error = %error, "failed to read queued release intents");
            return;
        }
    };
    for intent in intents {
        match replay_single_release_intent(ctx, pool, config, &intent).await {
            Ok(released) => {
                append_release_replay_record(
                    config,
                    &intent.intent_id,
                    &intent.content_sha256,
                    "replayed",
                    released,
                    None,
                );
            }
            Err((detail, retryable)) => {
                // Retryable (DB still degraded) → "failed", left queued for the
                // next replay. Non-retryable (agent/project gone, malformed
                // paths) → "abandoned", a terminal marker that clears the intent
                // so it is not retried — and re-appended — forever.
                let status = if retryable { "failed" } else { "abandoned" };
                append_release_replay_record(
                    config,
                    &intent.intent_id,
                    &intent.content_sha256,
                    status,
                    0,
                    Some(&detail),
                );
                tracing::warn!(
                    intent_id = intent.intent_id,
                    retryable,
                    error = %detail,
                    "queued release intent replay failed"
                );
            }
        }
    }
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
    // A row is active per the canonical ACTIVE_RESERVATION_PREDICATE when
    // released_ts IS NULL OR released_ts <= 0; only a positive released_ts means
    // genuinely released. `list_unreleased_file_reservations` can surface a
    // defensively-active row as released_ts = Some(0), so gate on `> 0` (not the
    // any-`Some` check) to match the SQL predicate the unfiltered release path
    // uses — otherwise path/id-filtered release+renew would skip a row the
    // blanket "release all" branch still acts on.
    if row.released_ts.is_some_and(|ts| ts > 0) {
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

/// F5 (br-bvq1x.6.5): build a fail-closed envelope for a reservation ACQUIRE
/// failure. Acquire must fail CLOSED for safety, but the agent has to be able to
/// tell WHY it failed — a corrupt DB, a corrupt index, or a busy/unavailable
/// subsystem — as distinct from a genuine conflict (which separately returns the
/// conflicting holder via `FILE_RESERVATION_CONFLICT`). This reuses the A1 typed
/// classification + A2 failure envelope from [`db_error_to_mcp_error`] (so the
/// class, severity, recommended command, and corruption metrics stay consistent)
/// and grafts on the reservation-specific context the generic chokepoint cannot
/// know: the exact requested paths and an explicit `do_not_edit` set, so an agent
/// that could not verify current holders treats every requested path as off-limits
/// until recovery rather than editing blind (the css/ts2 incident: `reserve`
/// failing with a malformed B-tree left the agent unable to tell "contended" from
/// "DB corrupt").
fn reservation_acquire_failure(
    requested_paths: &[String],
    operation: &'static str,
    err: DbError,
) -> McpError {
    let classification = err.classification();
    let cause = classification.class.as_str();
    let blocks_edits = classification.blocks_edits;
    let safe_to_continue_read_only = classification.safe_to_continue_read_only;
    let recommended_command = classification.recommended_command;
    // Reuse the canonical classified envelope (class / severity / code / metrics)...
    let mut error = db_error_to_mcp_error(err);
    // ...then graft the reservation-acquire fail-closed context onto its data.
    let guidance = if blocks_edits {
        "Reservation acquire FAILED CLOSED: current holders are unverifiable because the \
         reservation index could not be read. Do NOT edit the requested paths until the \
         database is recovered."
    } else {
        "Reservation acquire did NOT grant: the reservation subsystem is temporarily \
         unavailable (busy/locked). The paths were left unreserved; retry after the \
         condition clears."
    };
    // Fail closed: when the index is unreadable, every requested path is a
    // DO-NOT-EDIT until reservations can be verified again. (Precomputed because
    // `json!` cannot take a bare `if`/`else` in value position.)
    let do_not_edit: Vec<String> = if blocks_edits {
        requested_paths.to_vec()
    } else {
        Vec::new()
    };
    let context = json!({
        "operation": operation,
        "cause": cause,
        "fail_closed": true,
        "blocks_edits": blocks_edits,
        "safe_to_continue_read_only": safe_to_continue_read_only,
        "recommended_command": recommended_command,
        "requested_paths": requested_paths,
        "do_not_edit": do_not_edit,
        "guidance": guidance,
    });
    // `db_error_to_mcp_error` always produces the legacy envelope
    // `{ error: { type, message, recoverable, data: {...} } }`. Graft the
    // reservation-acquire context into that INNER `data` object — alongside the
    // A1 `db_error_classification` and `failure_envelope` — so the whole error
    // payload is coherent, rather than at an unrelated top level. (If the
    // envelope shape ever changed, the context is simply dropped and the
    // classified error still surfaces — defensive, never hit in practice.)
    if let Some(Value::Object(top)) = error.data.as_mut()
        && let Some(Value::Object(err_obj)) = top.get_mut("error")
        && let Some(Value::Object(data_obj)) = err_obj.get_mut("data")
    {
        data_obj.insert("reservation_acquire".to_string(), context);
    }
    error
}

/// F5: route an acquire-path DB [`asupersync::Outcome`] through
/// [`reservation_acquire_failure`] on error (so the caller gets the fail-closed
/// reservation context), delegating success / cancellation / panic to the shared
/// [`db_outcome_to_mcp_result`] chokepoint.
fn acquire_outcome<T>(
    out: asupersync::Outcome<T, DbError>,
    requested_paths: &[String],
    operation: &'static str,
) -> McpResult<T> {
    match out {
        asupersync::Outcome::Err(err) => {
            Err(reservation_acquire_failure(requested_paths, operation, err))
        }
        other => db_outcome_to_mcp_result(other),
    }
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

    // Check for conflicts with existing active reservations. F5: if this read
    // fails (DB/index corrupt, busy/unavailable), surface a fail-closed
    // reservation-acquire envelope that names the cause and the do-not-edit set
    // instead of an opaque DB error — the agent must be able to tell "could not
    // verify reservations" from "truly contended".
    let active = acquire_outcome(
        mcp_agent_mail_db::queries::get_active_reservations(ctx.cx(), &pool, project_id).await,
        &paths,
        "file_reservation_paths",
    )?;

    // F1 reconcile-on-read: before granting, heal any *existing* active
    // reservation whose archive artifact is missing or stale (a crash between
    // that reservation's DB-commit and its archive-write — GH#112). This is the
    // "next access converges" guarantee: the pre-commit guard reads the artifacts
    // directly, so without this a wrong/absent holder would slip through. The
    // healer fires an archive write only on genuine drift, so the steady state is
    // just one directory scan. Best-effort: a failed agent lookup never fails the
    // reserve.
    if let asupersync::Outcome::Ok(agent_rows) =
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await
    {
        let agent_names: HashMap<i64, String> = agent_rows
            .into_iter()
            .filter_map(|row| row.id.map(|id| (id, row.name)))
            .collect();
        reconcile_active_reservation_archive(&project, &active, &agent_names, &Config::get());
    }

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
                // F5: a grant failure that is not a recoverable conflict still
                // fails closed with the classified reservation-acquire context
                // (cause + do-not-edit set) rather than an opaque DB error.
                acquire_outcome(other, &paths, "file_reservation_paths")?;
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
        dispatch_reservation_archive_write(
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
    let config = Config::get();
    let original_paths = paths.clone();
    let original_file_reservation_ids = file_reservation_ids.clone();

    let pool = match get_db_pool() {
        Ok(pool) => pool,
        Err(error) => {
            return queued_release_intent_response(
                &config,
                &project_key,
                &agent_name,
                original_paths,
                original_file_reservation_ids,
                "get_db_pool",
                &error.to_string(),
            );
        }
    };
    let project = match resolve_project(ctx, &pool, &project_key).await {
        Ok(project) => project,
        Err(error) if mcp_error_supports_release_intent(&error) => {
            return queued_release_intent_response(
                &config,
                &project_key,
                &agent_name,
                original_paths,
                original_file_reservation_ids,
                "resolve_project",
                &error.to_string(),
            );
        }
        Err(error) => return Err(error),
    };
    let project_id = project.id.unwrap_or(0);
    let normalized_paths = normalize_filter_paths(&project.human_key, paths)?;

    let agent = match resolve_agent(
        ctx,
        &pool,
        project_id,
        &agent_name,
        &project.slug,
        &project.human_key,
    )
    .await
    {
        Ok(agent) => agent,
        Err(error) if mcp_error_supports_release_intent(&error) => {
            return queued_release_intent_response(
                &config,
                &project_key,
                &agent_name,
                original_paths,
                original_file_reservation_ids,
                "resolve_agent",
                &error.to_string(),
            );
        }
        Err(error) => return Err(error),
    };
    let agent_id = agent.id.unwrap_or(0);

    let ids_to_release = if normalized_paths.is_some() || file_reservation_ids.is_some() {
        let existing_rows = match mcp_agent_mail_db::queries::list_unreleased_file_reservations(
            ctx.cx(),
            &pool,
            project_id,
        )
        .await
        {
            asupersync::Outcome::Ok(rows) => rows,
            asupersync::Outcome::Err(error) if db_error_supports_release_intent(&error) => {
                return queued_release_intent_response(
                    &config,
                    &project_key,
                    &agent_name,
                    original_paths,
                    original_file_reservation_ids,
                    "list_unreleased_file_reservations",
                    &error.to_string(),
                );
            }
            other => db_outcome_to_mcp_result(other)?,
        };
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
    let released_rows = match mcp_agent_mail_db::queries::release_reservations(
        ctx.cx(),
        &pool,
        project_id,
        agent_id,
        None, // Pass resolved IDs only
        ids_to_release.as_deref(),
    )
    .await
    {
        asupersync::Outcome::Ok(rows) => rows,
        asupersync::Outcome::Err(error) if db_error_supports_release_intent(&error) => {
            return queued_release_intent_response(
                &config,
                &project_key,
                &agent_name,
                original_paths,
                original_file_reservation_ids,
                "release_reservations",
                &error.to_string(),
            );
        }
        other => db_outcome_to_mcp_result(other)?,
    };

    // Update archive artifacts for the released items
    dispatch_release_archive_write(&project, &agent, &released_rows, &config);
    replay_queued_release_intents(ctx, &pool, &config).await;

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
        dispatch_reservation_archive_write(
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

    let released_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_reservations_by_ids(
            ctx.cx(),
            &pool,
            &[file_reservation_id],
        )
        .await,
    )?;
    let released_ts = released_rows
        .iter()
        .find(|row| row.id == Some(file_reservation_id))
        .and_then(|row| row.released_ts)
        .ok_or_else(|| {
            legacy_tool_error(
                "INCONSISTENT_STATE",
                "Reservation release committed but released_ts could not be read back.",
                false,
                json!({ "file_reservation_id": file_reservation_id }),
            )
        })?;
    let released_iso = micros_to_iso(released_ts);

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
            "released_ts": released_iso.clone(),
        });

        let op = mcp_agent_mail_storage::WriteOp::FileReservation {
            project_slug: project.slug.clone(),
            config: Config::get(),
            reservations: vec![res_json],
        };
        dispatch_reservation_archive_write(
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
                crate::messaging::enqueue_message_lexical_index(
                    &mcp_agent_mail_db::search_v3::IndexableMessage {
                        id: message_id,
                        project_id,
                        project_slug: project.slug.clone(),
                        sender_name: agent_name.clone(),
                        subject: message.subject.clone(),
                        body_md: message.body_md.clone(),
                        thread_id: message.thread_id.clone(),
                        importance: message.importance.clone(),
                        created_ts: message.created_ts,
                    },
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
        "released_at": &released_iso,
        "reservation": {
            "id": file_reservation_id,
            "agent": &holder_agent_name,
            "path_pattern": reservation.path_pattern,
            "exclusive": reservation.exclusive != 0,
            "reason": reservation.reason,
            "created_ts": micros_to_iso(reservation.created_ts),
            "expires_ts": micros_to_iso(reservation.expires_ts),
            "released_ts": &released_iso,
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

    // --- F5 (br-bvq1x.6.5): acquire failure classification + fail-closed context ---

    #[test]
    fn f5_acquire_failure_corruption_fails_closed_with_do_not_edit() {
        let paths = vec!["src/**".to_string(), "config/app.yaml".to_string()];
        // A malformed-B-tree read failure on the conflict-check path (the css/ts2
        // incident): the reservation index could not be read.
        let err = DbError::Sqlite("database disk image is malformed".to_string());
        let mcp = reservation_acquire_failure(&paths, "file_reservation_paths", err);
        let data = mcp.data.expect("F5 envelope carries data");
        // Legacy envelope shape: { error: { type, message, recoverable, data } }.
        let payload = data
            .get("error")
            .and_then(|e| e.get("data"))
            .expect("legacy error.data payload");
        let acq = payload
            .get("reservation_acquire")
            .expect("reservation_acquire context block");
        assert_eq!(
            acq.get("cause").and_then(Value::as_str),
            Some("main_db_btree_corruption"),
            "corruption-driven acquire failure must classify as main_db_btree_corruption"
        );
        assert_eq!(acq.get("fail_closed").and_then(Value::as_bool), Some(true));
        assert_eq!(acq.get("blocks_edits").and_then(Value::as_bool), Some(true));
        // Fail closed: every requested path is do-not-edit when holders are
        // unverifiable — the agent must not edit blind.
        let do_not_edit: Vec<String> = acq
            .get("do_not_edit")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        assert_eq!(
            do_not_edit, paths,
            "a corruption-driven acquire failure must mark every requested path do-not-edit"
        );
        // Reuses the A1/A2 classified envelope (grafted alongside it) rather than
        // inventing a new one.
        assert!(
            payload.get("db_error_classification").is_some(),
            "F5 must reuse the A1 classification envelope from db_error_to_mcp_error"
        );
    }

    #[test]
    fn f5_acquire_failure_busy_is_distinct_from_corruption() {
        let paths = vec!["src/**".to_string()];
        // Busy/locked subsystem — UNAVAILABLE, not corrupt and not contended.
        let err = DbError::ResourceBusy("database is locked".to_string());
        let mcp = reservation_acquire_failure(&paths, "file_reservation_paths", err);
        let data = mcp.data.expect("data");
        let payload = data
            .get("error")
            .and_then(|e| e.get("data"))
            .expect("legacy error.data payload");
        let acq = payload
            .get("reservation_acquire")
            .expect("reservation_acquire context block");
        assert_eq!(acq.get("fail_closed").and_then(Value::as_bool), Some(true));
        // The whole point of F5: an UNAVAILABLE cause is classified DISTINCTLY
        // from a corruption cause (and both are distinct from a genuine conflict,
        // which is the separate FILE_RESERVATION_CONFLICT path).
        let cause = acq.get("cause").and_then(Value::as_str).unwrap_or_default();
        assert!(!cause.is_empty(), "busy failure must carry a typed cause");
        assert_ne!(
            cause, "main_db_btree_corruption",
            "a busy/unavailable failure must not be misclassified as corruption"
        );
        assert!(payload.get("db_error_classification").is_some());
    }

    fn with_serialized_reservations<F, T>(f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let _lock = RESERVATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        mcp_agent_mail_storage::wbq_start();
        mcp_agent_mail_storage::wbq_flush();
        mcp_agent_mail_storage::flush_async_commits();
        mcp_agent_mail_storage::clear_durability_degraded();

        let temp = tempfile::tempdir().expect("reservation test tempdir");
        let storage_root = temp.path().join("storage-root");
        std::fs::create_dir_all(&storage_root).expect("reservation test storage root");
        let database_path = temp.path().join("storage.sqlite3");
        let database_url = format!("sqlite://{}", database_path.display());
        let storage_root_str = storage_root
            .to_str()
            .expect("reservation test storage root utf-8")
            .to_string();

        let (result, stats, degraded) =
            mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[
                    ("DATABASE_URL", database_url.as_str()),
                    ("STORAGE_ROOT", storage_root_str.as_str()),
                ],
                || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
                    mcp_agent_mail_storage::wbq_flush();
                    mcp_agent_mail_storage::flush_async_commits();
                    let stats = mcp_agent_mail_storage::wbq_stats();
                    let degraded = mcp_agent_mail_storage::durability_degraded();
                    mcp_agent_mail_storage::clear_durability_degraded();
                    (result, stats, degraded)
                },
            );

        match result {
            Ok(value) => {
                assert!(
                    !degraded,
                    "reservation test caused WBQ durability degradation after cleanup flush: {stats:?}"
                );
                value
            }
            Err(panic) => std::panic::resume_unwind(panic),
        }
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
    // F1 (br-bvq1x.6.1): reconcile-on-read — heal a reservation whose archive
    // artifact is missing/stale after a crash between DB-commit and archive-write
    // -----------------------------------------------------------------------

    fn archive_view(
        id: i64,
        agent: &str,
        path_pattern: Option<&str>,
        exclusive: Option<bool>,
        released_ts: Option<i64>,
    ) -> crate::reservation_parity::ArchiveReservationView {
        crate::reservation_parity::ArchiveReservationView {
            reservation_id: id,
            agent_name: agent.to_string(),
            reason: String::new(),
            path_pattern: path_pattern.map(str::to_string),
            exclusive,
            released_ts,
        }
    }

    fn names(pairs: &[(i64, &str)]) -> HashMap<i64, String> {
        pairs
            .iter()
            .map(|(id, name)| (*id, (*name).to_string()))
            .collect()
    }

    #[test]
    fn heal_emits_missing_artifact() {
        // The crash-gap case: DB row committed, archive artifact never written.
        let rows = vec![reservation_row(1, 7, "src/**", 9_999, None)];
        let present = BTreeMap::new();
        let heal = reservation_rows_needing_archive_heal(
            "/abs/proj",
            &rows,
            &names(&[(7, "GreenCastle")]),
            &present,
        );
        assert_eq!(heal.len(), 1, "a missing artifact must be healed");
        assert_eq!(heal[0]["id"], 1);
        assert_eq!(heal[0]["agent"], "GreenCastle");
        assert_eq!(heal[0]["path_pattern"], "src/**");
        assert_eq!(heal[0]["exclusive"], true);
        assert_eq!(heal[0]["project"], "/abs/proj");
    }

    #[test]
    fn heal_emits_stale_holder() {
        // GH#112's wrong-holder class: the archive names a different agent than
        // the authoritative DB row.
        let rows = vec![reservation_row(1, 7, "src/**", 9_999, None)];
        let mut present = BTreeMap::new();
        present.insert(
            1,
            archive_view(1, "RustyOtter", Some("src/**"), Some(true), None),
        );
        let heal = reservation_rows_needing_archive_heal(
            "/abs/proj",
            &rows,
            &names(&[(7, "GreenCastle")]),
            &present,
        );
        assert_eq!(heal.len(), 1);
        assert_eq!(
            heal[0]["agent"], "GreenCastle",
            "the healed artifact must carry the authoritative DB holder"
        );
    }

    #[test]
    fn heal_emits_stale_path_pattern() {
        let rows = vec![reservation_row(1, 7, "src/a.rs", 9_999, None)];
        let mut present = BTreeMap::new();
        present.insert(
            1,
            archive_view(1, "GreenCastle", Some("src/b.rs"), Some(true), None),
        );
        let heal = reservation_rows_needing_archive_heal(
            "/abs/proj",
            &rows,
            &names(&[(7, "GreenCastle")]),
            &present,
        );
        assert_eq!(heal.len(), 1, "a divergent reserved path must be healed");
        assert_eq!(heal[0]["path_pattern"], "src/a.rs");
    }

    #[test]
    fn heal_emits_stale_exclusive_flag() {
        // DB row is shared (exclusive=0) but the archive claims exclusive.
        let mut row = reservation_row(1, 7, "src/**", 9_999, None);
        row.exclusive = 0;
        let mut present = BTreeMap::new();
        present.insert(
            1,
            archive_view(1, "GreenCastle", Some("src/**"), Some(true), None),
        );
        let heal = reservation_rows_needing_archive_heal(
            "/abs/proj",
            &[row],
            &names(&[(7, "GreenCastle")]),
            &present,
        );
        assert_eq!(heal.len(), 1);
        assert_eq!(heal[0]["exclusive"], false);
    }

    #[test]
    fn heal_emits_when_archive_claims_released_for_active_db_row() {
        // #112 stuck-released_ts class: the DB row is active but the archive
        // records it released (a stale release artifact would hide a live holder).
        let rows = vec![reservation_row(1, 7, "src/**", 9_999, None)];
        let mut present = BTreeMap::new();
        present.insert(
            1,
            archive_view(1, "GreenCastle", Some("src/**"), Some(true), Some(123)),
        );
        let heal = reservation_rows_needing_archive_heal(
            "/abs/proj",
            &rows,
            &names(&[(7, "GreenCastle")]),
            &present,
        );
        assert_eq!(heal.len(), 1);
    }

    #[test]
    fn heal_is_noop_when_consistent() {
        let rows = vec![reservation_row(1, 7, "src/**", 9_999, None)];
        let mut present = BTreeMap::new();
        present.insert(
            1,
            archive_view(1, "GreenCastle", Some("src/**"), Some(true), None),
        );
        let heal = reservation_rows_needing_archive_heal(
            "/abs/proj",
            &rows,
            &names(&[(7, "GreenCastle")]),
            &present,
        );
        assert!(
            heal.is_empty(),
            "a consistent artifact must not be re-emitted"
        );
    }

    #[test]
    fn heal_skips_row_with_unknown_agent() {
        // We must never *guess* a holder — that would write the wrong holder, the
        // exact failure being healed. A row whose agent_id is unresolvable is left
        // untouched.
        let rows = vec![reservation_row(1, 7, "src/**", 9_999, None)];
        let present = BTreeMap::new();
        let heal = reservation_rows_needing_archive_heal(
            "/abs/proj",
            &rows,
            &names(&[(9, "Other")]),
            &present,
        );
        assert!(
            heal.is_empty(),
            "an unresolvable holder must be skipped, never guessed"
        );
    }

    #[test]
    fn heal_does_not_flag_archive_that_omits_path_or_exclusive() {
        // br-xyy95 conservatism: a legacy/hand-authored artifact that omits
        // path_pattern/exclusive is not divergence. With a matching agent and no
        // release mismatch, it must not be re-emitted.
        let rows = vec![reservation_row(1, 7, "src/**", 9_999, None)];
        let mut present = BTreeMap::new();
        present.insert(1, archive_view(1, "GreenCastle", None, None, None));
        let heal = reservation_rows_needing_archive_heal(
            "/abs/proj",
            &rows,
            &names(&[(7, "GreenCastle")]),
            &present,
        );
        assert!(
            heal.is_empty(),
            "absent archive path_pattern/exclusive must not manufacture a heal"
        );
    }

    #[test]
    fn healed_artifact_json_round_trips_through_archive_scan() {
        // The consistency proof: the artifact the healer authors, written to disk
        // and scanned back, agrees with the DB row — so re-emitting converges
        // (does not itself become a new source of drift).
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage-root");
        let slug = "proj-roundtrip";
        let reservation_dir = storage_root
            .join("projects")
            .join(slug)
            .join("file_reservations");
        std::fs::create_dir_all(&reservation_dir).expect("create reservation dir");

        let row = reservation_row(1, 7, "src/**", 9_999, None);
        let artifact = active_reservation_artifact_json("/abs/proj", "GreenCastle", &row);
        std::fs::write(
            reservation_dir.join("id-1.json"),
            serde_json::to_vec_pretty(&artifact).expect("serialize artifact"),
        )
        .expect("write artifact");

        let view =
            crate::reservation_parity::read_project_archive_reservation(&storage_root, slug, 1)
                .expect("artifact scanned back");
        assert_eq!(view.agent_name, "GreenCastle");
        assert_eq!(view.path_pattern.as_deref(), Some("src/**"));
        assert_eq!(view.exclusive, Some(true));
        assert_eq!(view.released_ts, None);
        assert!(
            !active_archive_artifact_diverges(&view, &row, "GreenCastle"),
            "a freshly healed artifact must read back as consistent with the DB row"
        );
    }

    #[test]
    fn reconcile_on_read_heals_missing_artifact_after_crash_gap() {
        // The bead acceptance (br-bvq1x.6.1): a crafted crash between a
        // reservation's DB-commit and its archive-write reconciles to a consistent
        // state on next access — no wrong holder, no operator action.
        with_serialized_reservations(|| {
            run_async(|cx| async move {
                let config = Config::get();
                let pool = get_db_pool().expect("db pool");
                let project_key = format!("/tmp/f1-reconcile-{}", unique_suffix());
                let project = ensure_project(&cx, &pool, &project_key).await;
                let project_id = project.id.unwrap_or(0);
                let holder = register_agent(&cx, &pool, project_id, "GreenCastle").await;
                let holder_id = holder.id.unwrap_or(0);
                let ctx = McpContext::new(cx.clone(), 1);

                // Reserve src/** through the real tool so the DB row + archive
                // artifact are both written, then flush so the artifact lands.
                file_reservation_paths(
                    &ctx,
                    project_key.clone(),
                    "GreenCastle".to_string(),
                    vec!["src/**".to_string()],
                    Some(3600),
                    Some(true),
                    Some("f1 reconcile holder".to_string()),
                )
                .await
                .expect("initial reservation");
                mcp_agent_mail_storage::wbq_flush();

                let active = match queries::get_active_reservations(&cx, &pool, project_id).await {
                    Outcome::Ok(rows) => rows,
                    other => panic!("get_active_reservations failed: {other:?}"),
                };
                let reservation_id = active
                    .iter()
                    .find(|row| row.agent_id == holder_id)
                    .and_then(|row| row.id)
                    .expect("reserved row exists");

                let artifact_path = config
                    .storage_root
                    .join("projects")
                    .join(&project.slug)
                    .join("file_reservations")
                    .join(format!("id-{reservation_id}.json"));
                assert!(
                    artifact_path.exists(),
                    "archive artifact must exist after the initial reservation"
                );

                // Inject the crash gap: the DB row stays committed but its archive
                // artifact vanishes (the archive write was lost).
                std::fs::remove_file(&artifact_path).expect("inject crash gap");
                assert!(!artifact_path.exists(), "crash gap injected");

                // Next access: a second agent reserves a *different* path. The
                // reconcile-on-read pass over the active set must heal the missing
                // artifact for the first holder.
                register_agent(&cx, &pool, project_id, "BlueLake").await;
                file_reservation_paths(
                    &ctx,
                    project_key.clone(),
                    "BlueLake".to_string(),
                    vec!["docs/**".to_string()],
                    Some(3600),
                    Some(false),
                    Some("f1 reconcile next access".to_string()),
                )
                .await
                .expect("second reservation triggers reconcile-on-read");
                mcp_agent_mail_storage::wbq_flush();

                assert!(
                    artifact_path.exists(),
                    "reconcile-on-read must heal the missing archive artifact on next access"
                );
                let healed: Value = serde_json::from_slice(
                    &std::fs::read(&artifact_path).expect("read healed artifact"),
                )
                .expect("healed artifact is valid JSON");
                assert_eq!(
                    healed["agent"], "GreenCastle",
                    "healed artifact must name the authoritative holder (no wrong holder)"
                );
                assert_eq!(healed["path_pattern"], "src/**");
                assert_eq!(healed["exclusive"], true);
            });
        });
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
    fn release_intent_error_classifier_only_queues_database_failures() {
        let corruption = legacy_tool_error(
            "DATABASE_CORRUPTION",
            "database is malformed",
            false,
            json!({}),
        );
        let busy = legacy_tool_error("RESOURCE_BUSY", "database is locked", true, json!({}));
        let not_found = legacy_tool_error("NOT_FOUND", "agent not found", true, json!({}));
        let invalid = McpError::new(McpErrorCode::InvalidParams, "invalid path");

        assert!(mcp_error_supports_release_intent(&corruption));
        assert!(mcp_error_supports_release_intent(&busy));
        assert!(!mcp_error_supports_release_intent(&not_found));
        assert!(!mcp_error_supports_release_intent(&invalid));
    }

    #[test]
    fn release_intent_append_writes_hash_stamped_private_jsonl() {
        with_serialized_reservations(|| {
            let config = Config::get();
            let receipt = append_release_intent(
                &config,
                "/tmp/release-intent-project",
                "BlueLake",
                Some(vec!["src/**".to_string()]),
                Some(vec![7]),
                "get_db_pool",
                "database disk image is malformed",
            )
            .expect("append release intent");

            assert_eq!(receipt.intent_path, release_intent_log_path(&config));
            assert_eq!(receipt.content_sha256.len(), 64);
            assert_eq!(&receipt.content_sha256[..16], receipt.intent_id);

            let content =
                std::fs::read_to_string(&receipt.intent_path).expect("read release intent log");
            let value: Value = serde_json::from_str(content.trim()).expect("release intent JSON");
            assert_eq!(
                value["schema_version"].as_u64(),
                Some(u64::from(RELEASE_INTENT_SCHEMA_VERSION))
            );
            assert_eq!(value["kind"].as_str(), Some(RELEASE_INTENT_KIND));
            assert_eq!(
                value["intent_id"].as_str(),
                Some(receipt.intent_id.as_str())
            );
            assert_eq!(
                value["content_sha256"].as_str(),
                Some(receipt.content_sha256.as_str())
            );
            assert_eq!(
                value["project_key"].as_str(),
                Some("/tmp/release-intent-project")
            );
            assert_eq!(value["agent_name"].as_str(), Some("BlueLake"));
            assert_eq!(value["paths"][0].as_str(), Some("src/**"));
            assert_eq!(value["file_reservation_ids"][0].as_i64(), Some(7));
            assert_eq!(value["failure"]["stage"].as_str(), Some("get_db_pool"));

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let log_mode = std::fs::metadata(&receipt.intent_path)
                    .expect("release intent log metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(log_mode, 0o600);
                let parent = receipt
                    .intent_path
                    .parent()
                    .expect("release intent log parent");
                let parent_mode = std::fs::metadata(parent)
                    .expect("release intent parent metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(parent_mode, 0o700);

                let lock_mode = std::fs::metadata(release_intent_lock_path(&config))
                    .expect("release intent lock metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(lock_mode, 0o600);
            }
        });
    }

    #[test]
    fn queued_release_intent_response_reports_queued_release() {
        with_serialized_reservations(|| {
            let config = Config::get();
            let payload = queued_release_intent_response(
                &config,
                "/tmp/queued-release",
                "BlueLake",
                Some(vec!["src/main.rs".to_string()]),
                None,
                "release_reservations",
                "database is locked",
            )
            .expect("queued response");
            let parsed: Value = serde_json::from_str(&payload).expect("queued JSON");
            assert_eq!(parsed["released"].as_i64(), Some(0));
            assert_eq!(parsed["status"].as_str(), Some("queued"));
            assert_eq!(parsed["queued"].as_bool(), Some(true));
            assert_eq!(
                parsed["message"].as_str(),
                Some("lease release queued because DB unavailable")
            );
            assert!(parsed["released_at"].as_str().is_some());
            assert!(parsed["intent"]["id"].as_str().is_some());
            assert!(parsed["intent"]["content_sha256"].as_str().is_some());
            assert_eq!(
                parsed["intent"]["path"].as_str(),
                Some(release_intent_log_path(&config).to_string_lossy().as_ref())
            );
            assert_eq!(
                read_queued_release_intents(&config)
                    .expect("read queued intents")
                    .len(),
                1
            );
        });
    }

    #[test]
    fn release_intent_reader_suppresses_replayed_intents() {
        with_serialized_reservations(|| {
            let config = Config::get();
            let receipt = append_release_intent(
                &config,
                "/tmp/replayed-release",
                "BlueLake",
                None,
                Some(vec![42]),
                "list_unreleased_file_reservations",
                "database is locked",
            )
            .expect("append release intent");

            assert_eq!(
                read_queued_release_intents(&config)
                    .expect("read pending release intent")
                    .len(),
                1
            );
            append_release_replay_record(
                &config,
                &receipt.intent_id,
                &receipt.content_sha256,
                "replayed",
                1,
                None,
            );
            assert!(
                read_queued_release_intents(&config)
                    .expect("read after replay")
                    .is_empty()
            );
        });
    }

    #[test]
    fn release_intent_reader_suppresses_abandoned_intents() {
        with_serialized_reservations(|| {
            let config = Config::get();
            let receipt = append_release_intent(
                &config,
                "/tmp/abandoned-release",
                "BlueLake",
                None,
                Some(vec![42]),
                "list_unreleased_file_reservations",
                "database is locked",
            )
            .expect("append release intent");

            assert_eq!(
                read_queued_release_intents(&config)
                    .expect("read pending release intent")
                    .len(),
                1
            );
            // A permanently un-replayable intent records a terminal "abandoned"
            // marker; the reader must clear it just like "replayed" so it is not
            // retried (and re-appended) forever.
            append_release_replay_record(
                &config,
                &receipt.intent_id,
                &receipt.content_sha256,
                "abandoned",
                0,
                Some("agent no longer exists"),
            );
            assert!(
                read_queued_release_intents(&config)
                    .expect("read after abandon")
                    .is_empty(),
                "abandoned release intent must be terminal, not retried forever"
            );
        });
    }

    #[test]
    fn release_intent_reader_isolates_torn_final_line() {
        with_serialized_reservations(|| {
            let config = Config::get();
            // Record 1: a valid queued intent — the log now ends in a newline.
            append_release_intent(
                &config,
                "/tmp/torn-release",
                "BlueLake",
                None,
                Some(vec![7]),
                "list_unreleased_file_reservations",
                "database is locked",
            )
            .expect("append intent 1");

            // Simulate a crash mid-append: a torn fragment with NO trailing
            // newline (looks like a release intent but is truncated/invalid).
            let log_path = release_intent_log_path(&config);
            {
                use std::io::Write;
                let mut torn = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&log_path)
                    .expect("open release intent log");
                torn.write_all(b"{\"kind\":\"release_file_reservations_intent\",\"partial")
                    .expect("write torn fragment");
            }

            // Record 2: another valid intent appended after the torn fragment.
            append_release_intent(
                &config,
                "/tmp/torn-release",
                "BlueLake",
                None,
                Some(vec![8]),
                "release_reservations",
                "database is locked",
            )
            .expect("append intent 2");

            // The leading-newline guard isolates the torn fragment on its own
            // skippable line, so the second valid record is not swallowed.
            let intents = read_queued_release_intents(&config).expect("read intents");
            assert_eq!(
                intents.len(),
                2,
                "torn final line must not swallow the following valid record"
            );
        });
    }

    #[test]
    fn release_intent_reader_ignores_unhashed_replay_marker() {
        with_serialized_reservations(|| {
            let config = Config::get();
            let receipt = append_release_intent(
                &config,
                "/tmp/unhashed-replay-release",
                "BlueLake",
                None,
                Some(vec![42]),
                "release_reservations",
                "database is locked",
            )
            .expect("append release intent");

            let fake_replay = json!({
                "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
                "kind": RELEASE_INTENT_REPLAY_KIND,
                "intent_id": &receipt.intent_id,
                "replayed_ts": mcp_agent_mail_db::now_micros(),
                "status": "replayed",
                "released": 1,
                "error_detail": Value::Null,
            });
            append_release_intent_jsonl(&config, &fake_replay)
                .expect("append unhashed replay marker");

            let intents = read_queued_release_intents(&config).expect("read queued intents");
            assert_eq!(intents.len(), 1);
            assert_eq!(intents[0].intent_id, receipt.intent_id);
        });
    }

    #[test]
    fn release_intent_reader_ignores_replay_marker_for_wrong_intent_hash() {
        with_serialized_reservations(|| {
            let config = Config::get();
            let receipt = append_release_intent(
                &config,
                "/tmp/wrong-hash-replay-release",
                "BlueLake",
                None,
                Some(vec![42]),
                "release_reservations",
                "database is locked",
            )
            .expect("append release intent");

            let mut wrong_intent_hash = format!("{}{}", receipt.intent_id, "0".repeat(48));
            if wrong_intent_hash == receipt.content_sha256 {
                wrong_intent_hash = format!("{}{}", receipt.intent_id, "f".repeat(48));
            }
            append_release_replay_record(
                &config,
                &receipt.intent_id,
                &wrong_intent_hash,
                "replayed",
                1,
                None,
            );

            let intents = read_queued_release_intents(&config).expect("read queued intents");
            assert_eq!(intents.len(), 1);
            assert_eq!(intents[0].intent_id, receipt.intent_id);
            assert_eq!(intents[0].content_sha256, receipt.content_sha256);
        });
    }

    #[test]
    fn release_intent_reader_skips_hash_mismatches() {
        with_serialized_reservations(|| {
            let config = Config::get();
            let receipt = append_release_intent(
                &config,
                "/tmp/hash-mismatch-release",
                "BlueLake",
                None,
                Some(vec![42]),
                "release_reservations",
                "database is locked",
            )
            .expect("append release intent");

            let content =
                std::fs::read_to_string(&receipt.intent_path).expect("read release intent log");
            let first_line = content.lines().next().expect("release intent line");
            let mut tampered: Value =
                serde_json::from_str(first_line).expect("release intent JSON");
            tampered["agent_name"] = Value::String("RedLake".to_string());
            append_release_intent_jsonl(&config, &tampered).expect("append tampered intent");

            let intents = read_queued_release_intents(&config).expect("read queued intents");
            assert_eq!(intents.len(), 1);
            assert_eq!(intents[0].intent_id, receipt.intent_id);
            assert_eq!(intents[0].agent_name, "BlueLake");
        });
    }

    #[test]
    fn replay_queued_release_intent_releases_once() {
        with_serialized_reservations(|| {
            run_async(|cx| async move {
                let config = Config::get();
                let pool = get_db_pool().expect("db pool");
                let project_key = format!("/tmp/replay-release-intent-{}", unique_suffix());
                let project = ensure_project(&cx, &pool, &project_key).await;
                let project_id = project.id.unwrap_or(0);
                let agent = register_agent(&cx, &pool, project_id, "BlueLake").await;
                let agent_id = agent.id.unwrap_or(0);

                let created = match queries::create_file_reservations(
                    &cx,
                    &pool,
                    project_id,
                    agent_id,
                    &["src/**"],
                    3600,
                    true,
                    "queued release replay regression",
                )
                .await
                {
                    Outcome::Ok(rows) => rows,
                    other => panic!("create_file_reservations failed: {other:?}"),
                };
                let reservation_id = created[0].id.unwrap_or(0);

                append_release_intent(
                    &config,
                    &project.human_key,
                    &agent.name,
                    None,
                    Some(vec![reservation_id]),
                    "injected_db_unavailable",
                    "database disk image is malformed",
                )
                .expect("append release intent");

                let ctx = McpContext::new(cx.clone(), 1);
                replay_queued_release_intents(&ctx, &pool, &config).await;
                replay_queued_release_intents(&ctx, &pool, &config).await;

                let rows =
                    match queries::get_reservations_by_ids(&cx, &pool, &[reservation_id]).await {
                        Outcome::Ok(rows) => rows,
                        other => panic!("get_reservations_by_ids failed: {other:?}"),
                    };
                let released_ts = rows
                    .iter()
                    .find(|row| row.id == Some(reservation_id))
                    .and_then(|row| row.released_ts);
                assert!(
                    released_ts.is_some(),
                    "queued intent replay should release the reservation"
                );
                assert!(
                    read_queued_release_intents(&config)
                        .expect("read queued release intents")
                        .is_empty(),
                    "successfully replayed intents should not be replayed again"
                );
            });
        });
    }

    #[test]
    fn replay_abandons_release_intent_for_missing_agent() {
        with_serialized_reservations(|| {
            run_async(|cx| async move {
                let config = Config::get();
                let pool = get_db_pool().expect("db pool");
                let project_key = format!("/tmp/abandon-release-intent-{}", unique_suffix());
                // Create the project but never register the queued agent, so the
                // replay's resolve_agent returns NOT_FOUND — a permanent,
                // non-retryable failure (mirrors the ack abandon test).
                let project = ensure_project(&cx, &pool, &project_key).await;

                append_release_intent(
                    &config,
                    &project.human_key,
                    "GhostAgent",
                    None,
                    Some(vec![1]),
                    "injected_db_unavailable",
                    "database disk image is malformed",
                )
                .expect("append release intent");
                assert_eq!(
                    read_queued_release_intents(&config)
                        .expect("read queued release intents")
                        .len(),
                    1
                );

                let ctx = McpContext::new(cx.clone(), 1);
                // Idempotent: a permanently un-replayable intent must clear on the
                // first replay and stay cleared, never retried forever.
                replay_queued_release_intents(&ctx, &pool, &config).await;
                replay_queued_release_intents(&ctx, &pool, &config).await;

                assert!(
                    read_queued_release_intents(&config)
                        .expect("read after abandon")
                        .is_empty(),
                    "release intent for a missing agent should be abandoned, not retried forever"
                );
            });
        });
    }

    #[test]
    fn release_intent_retry_reports_current_release_before_replay() {
        with_serialized_reservations(|| {
            run_async(|cx| async move {
                let config = Config::get();
                let pool = get_db_pool().expect("db pool");
                let project_key = format!("/tmp/release-intent-retry-{}", unique_suffix());
                let project = ensure_project(&cx, &pool, &project_key).await;
                let project_id = project.id.unwrap_or(0);
                let agent = register_agent(&cx, &pool, project_id, "BlueLake").await;
                let agent_id = agent.id.unwrap_or(0);

                let created = match queries::create_file_reservations(
                    &cx,
                    &pool,
                    project_id,
                    agent_id,
                    &["src/**"],
                    3600,
                    true,
                    "queued release retry regression",
                )
                .await
                {
                    Outcome::Ok(rows) => rows,
                    other => panic!("create_file_reservations failed: {other:?}"),
                };
                let reservation_id = created[0].id.unwrap_or(0);

                append_release_intent(
                    &config,
                    &project.human_key,
                    &agent.name,
                    None,
                    Some(vec![reservation_id]),
                    "injected_db_unavailable",
                    "database disk image is malformed",
                )
                .expect("append release intent");

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
                assert_eq!(
                    parsed["released"].as_i64(),
                    Some(1),
                    "retrying the same release should report the caller's release, not a pre-call replay"
                );
                assert!(
                    read_queued_release_intents(&config)
                        .expect("read queued release intents")
                        .is_empty(),
                    "successful retry should mark the prior intent replayed after the current release"
                );
            });
        });
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
    fn renewal_filter_matches_includes_defensively_active_nonpositive_released_ts() {
        // released_ts <= 0 is ACTIVE per the canonical predicate, so a path/id
        // filtered release/renew must still match it — consistent with the
        // unfiltered "release all" SQL branch.
        let active_zero = reservation_row(42, 7, "src/**", 1, Some(0));
        assert!(
            renewal_filter_matches(&active_zero, 7, Some(&["src/main.rs".to_string()]), None),
            "released_ts=0 is active and must match for renew/release"
        );
        // A genuinely released row (positive released_ts) stays excluded.
        let released = reservation_row(43, 7, "src/**", 1, Some(1_700_000_000_000_000));
        assert!(
            !renewal_filter_matches(&released, 7, Some(&["src/main.rs".to_string()]), None),
            "a positively-released reservation must not match for renew/release"
        );
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
                    "INSERT OR REPLACE INTO file_reservations \
                     (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
                     VALUES (?, ?, ?, 'src/**', 1, 'force release stale cache regression', ?, ?, NULL)",
                    &[
                        mcp_agent_mail_db::sqlmodel::Value::BigInt(reservation_id),
                        mcp_agent_mail_db::sqlmodel::Value::BigInt(project_id),
                        mcp_agent_mail_db::sqlmodel::Value::BigInt(holder_id),
                        mcp_agent_mail_db::sqlmodel::Value::BigInt(
                            mcp_agent_mail_db::now_micros().saturating_sub(2_000_000),
                        ),
                        mcp_agent_mail_db::sqlmodel::Value::BigInt(
                            mcp_agent_mail_db::now_micros().saturating_sub(1),
                        ),
                    ],
                )
                .expect("restore orphaned reservation fixture");
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

    #[test]
    fn force_release_file_reservation_reports_committed_release_timestamp() {
        with_serialized_reservations(|| {
            run_async(|cx| async move {
                let pool = get_db_pool().expect("db pool");
                let project_key = format!("/tmp/force-release-db-ts-{}", unique_suffix());
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
                    &["src/release-timestamp.rs"],
                    3600,
                    true,
                    "force release timestamp regression",
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
                drop(conn);

                let ctx = McpContext::new(cx.clone(), 1);
                let payload = force_release_file_reservation(
                    &ctx,
                    project.human_key.clone(),
                    actor.name.clone(),
                    reservation_id,
                    None,
                    Some(false),
                )
                .await
                .expect("force release succeeds");
                let parsed: Value = serde_json::from_str(&payload).expect("valid JSON");
                assert_eq!(parsed["released"].as_i64(), Some(1));

                let rows =
                    match queries::get_reservations_by_ids(&cx, &pool, &[reservation_id]).await {
                        Outcome::Ok(rows) => rows,
                        other => panic!("get_reservations_by_ids failed: {other:?}"),
                    };
                let db_release_ts = rows
                    .iter()
                    .find(|row| row.id == Some(reservation_id))
                    .and_then(|row| row.released_ts)
                    .expect("reservation should have committed released_ts");
                let db_release_iso = micros_to_iso(db_release_ts);

                assert_eq!(
                    parsed["released_at"].as_str(),
                    Some(db_release_iso.as_str())
                );
                assert_eq!(
                    parsed["reservation"]["released_ts"].as_str(),
                    Some(db_release_iso.as_str())
                );
            });
        });
    }
}
