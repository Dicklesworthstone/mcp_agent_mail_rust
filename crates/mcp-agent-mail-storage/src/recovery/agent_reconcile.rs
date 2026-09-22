//! Bounded recovery of agent profiles from the live mailbox.
//!
//! Lifecycle transitions commit to SQLite before their best-effort archive
//! write. If both the queue and its journal are lost, reconstructing an old
//! profile can restore an identity that was retired or deregistered. This
//! maintenance pass repairs those profiles without changing mailbox rows or
//! relying on another registration, message, or client read.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use asupersync::{Cx, Outcome};
use fastmcp_core::block_on;
use git2::{ErrorCode, ObjectType, Oid, Repository, Tree};
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbError, DbPool, corruption_circuit_breaker};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{ProjectArchive, StorageError};

const IDS_PER_BATCH: i64 = 32;
const MAX_REPAIRS_PER_BATCH: usize = 4;
const MAX_PROFILE_BYTES: usize = 128 * 1024;

/// A finite round prevents continuously registered identities from starving
/// old lifecycle changes. Restarting or changing the live pool starts a new
/// round; cursors never authorize a write or a deletion.
#[derive(Debug, Default)]
pub struct AgentReconcileCursor {
    source_identity: String,
    after: i64,
    ceiling: Option<i64>,
}

/// Counts describe one bounded pass, not whole-mailbox durability.
#[derive(Debug, Default, Serialize)]
pub struct AgentReconcileReport {
    pub scanned: usize,
    pub unchanged: usize,
    pub repaired: usize,
    pub deferred: usize,
    pub interrupted: bool,
    pub budget_exhausted: bool,
}

#[derive(Debug, PartialEq)]
struct AgentSource {
    id: i64,
    project_slug: String,
    project_key: String,
    profile: Value,
}

fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::InvalidPath(message.into())
}

fn source_error(error: impl std::fmt::Display) -> String {
    let error = DbError::Sqlite(error.to_string());
    corruption_circuit_breaker().observe_error(&error);
    format!("agent reconciliation source query failed: {error}")
}

fn outcome<T, E: std::fmt::Display>(value: Outcome<T, E>) -> Result<T, String> {
    match value {
        Outcome::Ok(value) => Ok(value),
        Outcome::Err(error) => Err(source_error(error)),
        Outcome::Cancelled(_) => Err("agent reconciliation source read cancelled".into()),
        Outcome::Panicked(_) => Err("agent reconciliation source read panicked".into()),
    }
}

fn validate_source(pool: &DbPool, config: &Config) -> Result<(), String> {
    let selected =
        mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url)
            .ok_or("agent reconciliation requires a file-backed source")?;
    let selected = fs::canonicalize(selected).map_err(|error| error.to_string())?;
    let source = fs::canonicalize(pool.sqlite_path()).map_err(|error| error.to_string())?;
    if source != selected || pool.search_identity_path() != pool.sqlite_path() {
        return Err("agent reconciliation pool is not the configured live database".into());
    }
    let pool_root = fs::canonicalize(pool.storage_root()).map_err(|error| error.to_string())?;
    let configured_root =
        fs::canonicalize(&config.storage_root).map_err(|error| error.to_string())?;
    if pool_root != configured_root {
        return Err("agent reconciliation pool and archive roots do not match".into());
    }
    Ok(())
}

fn select_ids(
    cx: &Cx,
    pool: &DbPool,
    cursor: &mut AgentReconcileCursor,
) -> Result<Vec<i64>, String> {
    let identity = pool.sqlite_identity_key();
    if cursor.source_identity != identity {
        *cursor = AgentReconcileCursor {
            source_identity: identity,
            ..Default::default()
        };
    }
    let conn = outcome(block_on(pool.acquire(cx)))?;
    let mode = conn
        .query_sync("PRAGMA query_only", &[])
        .map_err(source_error)?;
    if mode
        .first()
        .ok_or("source query-only mode was not reported")?
        .get_as::<i64>(0)
        .map_err(source_error)?
        != 0
    {
        return Err("query-only snapshots cannot authorize agent archive repair".into());
    }
    if cursor.ceiling.is_none() {
        let rows = conn
            .query_sync("SELECT COALESCE(MAX(id), 0) AS max_id FROM agents", &[])
            .map_err(source_error)?;
        cursor.ceiling = Some(
            rows.first()
                .ok_or("agent ID aggregate returned no row")?
                .get_named::<i64>("max_id")
                .map_err(source_error)?,
        );
        cursor.after = 0;
    }
    let rows = conn
        .query_sync(
            "SELECT id FROM agents WHERE id > ? AND id <= ? ORDER BY id ASC LIMIT ?",
            &[
                cursor.after.into(),
                cursor.ceiling.unwrap_or(0).into(),
                IDS_PER_BATCH.into(),
            ],
        )
        .map_err(source_error)?;
    if rows.is_empty() {
        cursor.ceiling = None;
    }
    rows.into_iter()
        .map(|row| row.get_named::<i64>("id").map_err(source_error))
        .collect()
}

fn timestamp(value: i64) -> Result<String, String> {
    if chrono::DateTime::from_timestamp_micros(value).is_none() {
        return Err("agent timestamp cannot be represented faithfully".into());
    }
    Ok(mcp_agent_mail_db::micros_to_iso(value))
}

fn read_source(cx: &Cx, pool: &DbPool, id: i64) -> Result<AgentSource, String> {
    let conn = outcome(block_on(pool.acquire(cx)))?;
    // One statement observes identity, lifecycle ledger and project together.
    // Do not select registration_token, even temporarily. The raw text bound
    // precedes materialization; serialization has its own bound below.
    let rows = conn.query_sync(
        "SELECT a.id, a.name, a.program, a.model, a.task_description, \
                a.inception_ts, a.last_active_ts, a.attachments_policy, a.contact_policy, \
                a.reaper_exempt, a.retired_at, d.deregistered_at, \
                p.slug AS project_slug, p.human_key AS project_key \
         FROM agents a JOIN projects p ON p.id = a.project_id \
         LEFT JOIN agent_deregistrations d ON d.agent_id = a.id \
         WHERE a.id = ? AND \
               length(CAST(a.name AS BLOB)) + length(CAST(a.program AS BLOB)) + \
               length(CAST(a.model AS BLOB)) + length(CAST(a.task_description AS BLOB)) + \
               length(CAST(a.attachments_policy AS BLOB)) + length(CAST(a.contact_policy AS BLOB)) + \
               length(CAST(p.slug AS BLOB)) + length(CAST(p.human_key AS BLOB)) <= ? \
               AND NOT EXISTS (SELECT 1 FROM agents other WHERE other.project_id = a.project_id \
                   AND other.id != a.id AND other.name = a.name COLLATE NOCASE)",
        &[id.into(), (MAX_PROFILE_BYTES as i64).into()],
    ).map_err(source_error)?;
    let [row] = rows.as_slice() else {
        return Err(
            "agent missing, oversized, or lacking unambiguous project/name authority".into(),
        );
    };
    if id <= 0 || row.get_named::<i64>("id").map_err(source_error)? != id {
        return Err("agent source identity does not match the requested ID".into());
    }
    let text = |name| row.get_named::<String>(name).map_err(source_error);
    let name = text("name")?;
    let project_slug = text("project_slug")?;
    crate::validate_archive_component("agent name", &name).map_err(|error| error.to_string())?;
    crate::validate_archive_component("project slug", &project_slug)
        .map_err(|error| error.to_string())?;
    let project_key = text("project_key")?;
    if !Path::new(&project_key).is_absolute() {
        return Err("agent project human_key must be absolute".into());
    }
    let retired_at = row
        .get_named::<Option<i64>>("retired_at")
        .map_err(source_error)?;
    let deregistered_at = row
        .get_named::<Option<i64>>("deregistered_at")
        .map_err(source_error)?;
    let reaper_exempt = row
        .get_named::<i64>("reaper_exempt")
        .map_err(source_error)?;
    if !matches!(reaper_exempt, 0 | 1) {
        return Err("agent reaper exemption is not boolean".into());
    }
    let profile = json!({
        "name": name,
        "program": text("program")?,
        "model": text("model")?,
        "task_description": text("task_description")?,
        "inception_ts": timestamp(row.get_named::<i64>("inception_ts").map_err(source_error)?)?,
        "last_active_ts": timestamp(row.get_named::<i64>("last_active_ts").map_err(source_error)?)?,
        "attachments_policy": text("attachments_policy")?,
        "contact_policy": text("contact_policy")?,
        "reaper_exempt": reaper_exempt != 0,
        "retired_at": retired_at.map(timestamp).transpose()?,
        "deregistered_at": deregistered_at.map(timestamp).transpose()?,
    });
    if serde_json::to_vec(&profile)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_PROFILE_BYTES
    {
        return Err("serialized agent profile exceeds the byte bound".into());
    }
    Ok(AgentSource {
        id,
        project_slug,
        project_key,
        profile,
    })
}

struct Artifact {
    bytes: Vec<u8>,
    value: Value,
}

fn decode_artifact(bytes: Vec<u8>) -> crate::Result<Artifact> {
    if bytes.len() > MAX_PROFILE_BYTES {
        return Err(invalid("agent metadata exceeds the artifact byte bound"));
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    if !value.is_object() {
        return Err(invalid(
            "agent metadata must be a JSON object; existing evidence retained",
        ));
    }
    Ok(Artifact { bytes, value })
}

fn read_artifact(path: &Path) -> crate::Result<Option<Artifact>> {
    if crate::path_existing_prefix_has_symlink(path)? {
        return Err(invalid("agent metadata path contains a symlink"));
    }
    let file = match mcp_agent_mail_core::disk::open_regular_file_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_PROFILE_BYTES as u64 {
        return Err(invalid("agent metadata is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PROFILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    decode_artifact(bytes).map(Some)
}

fn head_tree(repo: &Repository) -> crate::Result<Option<Tree<'_>>> {
    match repo.head() {
        Ok(head) => Ok(Some(head.peel_to_tree()?)),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn committed_artifact(
    repo: &Repository,
    tree: Option<&Tree<'_>>,
    path: &str,
) -> crate::Result<Option<Artifact>> {
    let Some(tree) = tree else { return Ok(None) };
    let entry = match tree.get_path(Path::new(path)) {
        Ok(entry) => entry,
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if entry.kind() != Some(ObjectType::Blob) || !matches!(entry.filemode(), 0o100644 | 0o100755) {
        return Err(invalid("committed agent metadata is not a regular file"));
    }
    let (size, kind) = repo.odb()?.read_header(entry.id())?;
    if kind != ObjectType::Blob || size > MAX_PROFILE_BYTES {
        return Err(invalid(
            "committed agent metadata exceeds the artifact byte bound",
        ));
    }
    decode_artifact(repo.find_blob(entry.id())?.content().to_vec()).map(Some)
}

fn profile_timestamp(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(value) => value
            .trim()
            .parse()
            .ok()
            .or_else(|| mcp_agent_mail_core::iso_to_micros(value.trim())),
        _ => None,
    }
}

fn validate_profile(source: &AgentSource, profile: &Value) -> crate::Result<()> {
    if profile.get("name") != source.profile.get("name") {
        return Err(invalid(
            "archived agent name conflicts with the live identity; preserved",
        ));
    }
    for field in ["inception_ts", "registered_ts"] {
        if let Some(value) = profile.get(field).filter(|value| !value.is_null()) {
            let observed = profile_timestamp(value);
            let expected = profile_timestamp(&source.profile["inception_ts"]);
            if observed.is_none() || observed != expected {
                return Err(invalid(
                    "archived agent inception conflicts with the live identity; preserved",
                ));
            }
        }
    }
    // Deregistration is permanent. A missing live ledger cannot authorize
    // clearing a surviving tombstone, even after a damaged DB was admitted.
    if let Some(value) = profile
        .get("deregistered_at")
        .filter(|value| !value.is_null())
    {
        let observed = profile_timestamp(value);
        let expected = profile_timestamp(&source.profile["deregistered_at"]);
        if observed.is_none() || observed != expected {
            return Err(invalid(
                "archived deregistration conflicts with the live ledger; preserved",
            ));
        }
    }
    if source.profile["deregistered_at"].is_null()
        && profile
            .get("task_description")
            .and_then(Value::as_str)
            .is_some_and(
                mcp_agent_mail_db::models::task_description_uses_reserved_deregistered_prefix,
            )
        && profile
            .get("contact_policy")
            .and_then(Value::as_str)
            .is_some_and(|policy| policy.eq_ignore_ascii_case("block_all"))
    {
        return Err(invalid(
            "archived legacy deregistration conflicts with the live ledger; preserved",
        ));
    }
    Ok(())
}

fn validate_project(source: &AgentSource, metadata: &Value) -> crate::Result<()> {
    let key = metadata.get("human_key").and_then(Value::as_str);
    if metadata.get("slug").and_then(Value::as_str) != Some(source.project_slug.as_str())
        || key.is_none_or(|key| {
            !Path::new(key).is_absolute()
                || mcp_agent_mail_core::resolve_project_path(key)
                    != mcp_agent_mail_core::resolve_project_path(&source.project_key)
        })
    {
        return Err(invalid(
            "archived project authority conflicts with the live agent; preserved",
        ));
    }
    Ok(())
}

fn merge_metadata(
    committed: Option<&Artifact>,
    working: Option<&Artifact>,
    authoritative: &Value,
    profile: bool,
) -> Value {
    let mut merged = committed
        .map(|artifact| artifact.value.clone())
        .unwrap_or_else(|| json!({}));
    let object = merged.as_object_mut().expect("validated metadata object");
    if let Some(working) = working {
        object.extend(
            working
                .value
                .as_object()
                .expect("validated working object")
                .clone(),
        );
    }
    let activity_only = profile
        && authoritative.as_object().is_some_and(|fields| {
            fields
                .iter()
                .filter(|(key, _)| key.as_str() != "last_active_ts")
                .all(|(key, value)| object.get(key) == Some(value))
        });
    for (key, value) in authoritative
        .as_object()
        .expect("authoritative metadata object")
    {
        // Activity flushes are frequent. They do not warrant another Git
        // commit when the archived identity and lifecycle already agree.
        if key != "last_active_ts" || !activity_only || !object.contains_key(key) {
            object.insert(key.clone(), value.clone());
        }
    }
    object.remove("registration_token");
    merged
}

fn serialized_metadata(value: &Value) -> crate::Result<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(value)?;
    if bytes.len() > MAX_PROFILE_BYTES {
        return Err(invalid(
            "merged agent metadata exceeds the artifact byte bound",
        ));
    }
    Ok(bytes)
}

fn reconcile_agent(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    original: &AgentSource,
) -> Result<bool, String> {
    let _mutation = crate::ArchiveMutationGuard::begin_at(&config.storage_root);
    let archive =
        crate::ensure_archive(config, &original.project_slug).map_err(|error| error.to_string())?;
    crate::with_project_lock(&archive, || {
        // Lock waits must not publish an already-obsolete snapshot. Release
        // the SQL connection before file/Git I/O. A later lifecycle mutation
        // is intentionally reconciled on the next finite round.
        let source = read_source(cx, pool, original.id).map_err(invalid)?;
        if source.project_slug != original.project_slug
            || source.project_key != original.project_key
            || source.profile["name"] != original.profile["name"]
            || source.profile["inception_ts"] != original.profile["inception_ts"]
        {
            return Err(invalid(
                "agent identity changed while awaiting archive publication; deferred",
            ));
        }
        publish_profile(&archive, config, &source)
    })
    .map_err(|error| error.to_string())
}

fn publish_profile(
    archive: &ProjectArchive,
    config: &Config,
    source: &AgentSource,
) -> crate::Result<bool> {
    let root = crate::archive_project_root_checked(archive)?;
    let name = source.profile["name"]
        .as_str()
        .ok_or_else(|| invalid("agent source lacks name"))?;
    let profile_path = root.join("agents").join(name).join("profile.json");
    let project_path = root.join("project.json");
    let profile_rel = crate::rel_path_cached(&archive.canonical_repo_root, &profile_path)?;
    let project_rel = crate::rel_path_cached(&archive.canonical_repo_root, &project_path)?;
    let repo_root = crate::archive_repo_root_checked(archive)?;
    let repo = Repository::open(repo_root)?;
    let tree = head_tree(&repo)?;
    let working_profile = read_artifact(&profile_path)?;
    let committed_profile = committed_artifact(&repo, tree.as_ref(), &profile_rel)?;
    let working_project = read_artifact(&project_path)?;
    let committed_project = committed_artifact(&repo, tree.as_ref(), &project_rel)?;
    for artifact in [working_profile.as_ref(), committed_profile.as_ref()]
        .into_iter()
        .flatten()
    {
        validate_profile(source, &artifact.value)?;
    }
    for artifact in [working_project.as_ref(), committed_project.as_ref()]
        .into_iter()
        .flatten()
    {
        validate_project(source, &artifact.value)?;
    }
    let profile = merge_metadata(
        committed_profile.as_ref(),
        working_profile.as_ref(),
        &source.profile,
        true,
    );
    let project = merge_metadata(
        committed_project.as_ref(),
        working_project.as_ref(),
        &json!({
            "slug": source.project_slug, "human_key": source.project_key,
        }),
        false,
    );
    let profile_bytes = serialized_metadata(&profile)?;
    let project_bytes = serialized_metadata(&project)?;
    let targets = [
        (
            &profile_path,
            &profile_rel,
            &profile_bytes,
            working_profile.as_ref(),
            committed_profile.as_ref(),
        ),
        (
            &project_path,
            &project_rel,
            &project_bytes,
            working_project.as_ref(),
            committed_project.as_ref(),
        ),
    ];
    if targets.iter().all(|(_, _, bytes, working, committed)| {
        working.is_some_and(|artifact| artifact.bytes == **bytes)
            && committed.is_some_and(|artifact| artifact.bytes == **bytes)
    }) {
        return Ok(false);
    }
    // All artifacts and source authority are validated before the first
    // replacement. Unrelated staged files never enter this explicit path set.
    for (path, _, bytes, working, _) in &targets {
        if working.is_none_or(|artifact| artifact.bytes != **bytes) {
            crate::ensure_parent_dir(path)?;
            crate::atomic_write_bytes(path, bytes, true)?;
        }
    }
    crate::commit_paths_with_retry(
        repo_root,
        config,
        &format!("repair: reconcile agent {name} profile"),
        &[&profile_rel, &project_rel],
    )?;
    let verified = Repository::open(repo_root)?;
    let verified_tree =
        head_tree(&verified)?.ok_or_else(|| invalid("agent profile commit lacks HEAD"))?;
    for (_, relative, bytes, _, _) in targets {
        let entry = verified_tree.get_path(Path::new(relative))?;
        if entry.kind() != Some(ObjectType::Blob)
            || !matches!(entry.filemode(), 0o100644 | 0o100755)
            || entry.id() != Oid::hash_object(ObjectType::Blob, bytes)?
        {
            return Err(invalid(
                "agent profile Git verification failed; files retained",
            ));
        }
        // A tree reference alone cannot prove that its object survived a
        // failed/corrupt object write. Verify the stored payload as well.
        let blob = verified.find_blob(entry.id())?;
        if blob.content() != bytes.as_slice() {
            return Err(invalid(
                "agent profile Git blob differs from repaired bytes; files retained",
            ));
        }
    }
    Ok(true)
}

/// Repair at most four profiles while inspecting at most 32 identities.
///
/// Uses only the configured live pool. Missing/ambiguous source rows,
/// conflicting identity evidence, symlinks and oversized metadata defer;
/// another finite round retries them. Tokens are never selected or archived.
/// SQL connections are released before filesystem/Git operations, and each
/// successful repair verifies its exact bytes in Git HEAD. A concurrent later
/// lifecycle change can require a later pass; this is eventual convergence.
///
/// # Errors
///
/// Refuses foreign or query-only sources, source errors and open corruption
/// breakers. Per-identity failures retain evidence and count as deferred.
pub fn reconcile_agent_batch(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut AgentReconcileCursor,
    stop: &AtomicBool,
) -> Result<AgentReconcileReport, String> {
    let mut report = AgentReconcileReport::default();
    if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
        report.interrupted = true;
        return Ok(report);
    }
    if corruption_circuit_breaker().is_tripped() {
        return Err("agent reconciliation refused: source corruption breaker is open".into());
    }
    validate_source(pool, config)?;
    for id in select_ids(cx, pool, cursor)? {
        if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
            report.interrupted = true;
            break;
        }
        if corruption_circuit_breaker().is_tripped() {
            return Err("agent reconciliation stopped: source corruption observed".into());
        }
        if report.repaired >= MAX_REPAIRS_PER_BATCH {
            report.budget_exhausted = true;
            break;
        }
        let _write_activity = mcp_agent_mail_db::write_barrier::begin_write_activity();
        let result =
            read_source(cx, pool, id).and_then(|source| reconcile_agent(cx, pool, config, &source));
        report.scanned += 1;
        match result {
            Ok(true) => report.repaired += 1,
            Ok(false) => report.unchanged += 1,
            Err(reason) => {
                report.deferred += 1;
                tracing::warn!(
                    target: "maintenance", event = "agent_archive_reconcile_deferred",
                    agent_id = id, reason = %reason,
                    "agent profile repair deferred; mailbox and conflicting evidence retained"
                );
            }
        }
        cursor.after = id;
        if cursor.ceiling == Some(id) {
            cursor.ceiling = None;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_mailbox(test: impl FnOnce(&Cx, &DbPool, &Config, &Path)) {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let db_path = temp.path().join("mail.sqlite3");
            let storage_root = temp.path().join("archive");
            fs::create_dir_all(&storage_root).unwrap();
            let config = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&db_path),
                storage_root,
                ..Config::default()
            };
            let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(101, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts, registration_token) \
                VALUES(101, 101, 'BlueLake', 'test', 'test', 'coordination', 1000000, 2000000, 'never-archive-this-secret')").unwrap();
            drop(conn);
            test(&cx, &pool, &config, temp.path());
        });
    }

    fn reconcile(cx: &Cx, pool: &DbPool, config: &Config) -> AgentReconcileReport {
        reconcile_agent_batch(
            cx,
            pool,
            config,
            &mut AgentReconcileCursor::default(),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    fn profile_path(config: &Config) -> std::path::PathBuf {
        config
            .storage_root
            .join("projects/project/agents/BlueLake/profile.json")
    }

    fn assert_profile_committed(config: &Config) -> Value {
        let path = profile_path(config);
        let bytes = fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("never-archive-this-secret"));
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("registration_token").is_none());
        let repo = Repository::open(&config.storage_root).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let entry = tree
            .get_path(Path::new("projects/project/agents/BlueLake/profile.json"))
            .unwrap();
        assert_eq!(repo.find_blob(entry.id()).unwrap().content(), bytes);
        value
    }

    #[test]
    fn real_lifecycle_profiles_converge_after_lost_archive_writes_and_reopen() {
        with_mailbox(|cx, pool, config, scratch| {
            assert_eq!(reconcile(cx, pool, config).repaired, 1);
            let active = assert_profile_committed(config);
            let mut extended = active.clone();
            extended["extension"] = json!({"review": ["preserve", "this"]});
            crate::write_json(&profile_path(config), &extended, true).unwrap();
            crate::commit_paths_with_retry(
                &config.storage_root,
                config,
                "fixture: retain profile extension",
                &["projects/project/agents/BlueLake/profile.json"],
            )
            .unwrap();

            outcome(block_on(mcp_agent_mail_db::queries::set_agent_retired_at(
                cx,
                pool,
                101,
                Some(123_456_789),
            )))
            .unwrap();
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE agents SET reaper_exempt = 1 WHERE id = 101")
                .unwrap();
            drop(conn);
            // No lifecycle archive op is delivered. A reopened live pool and
            // new maintenance cursor must independently recover the DB state.
            let reopened = mcp_agent_mail_db::DbPool::new(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                run_migrations: false,
                ..Default::default()
            })
            .unwrap();
            let report = reconcile(cx, &reopened, config);
            assert_eq!(report.repaired, 1);
            assert_eq!(report.deferred, 0);
            let retired = assert_profile_committed(config);
            assert_eq!(retired["retired_at"], timestamp(123_456_789).unwrap());
            assert_eq!(retired["reaper_exempt"], true);
            assert_eq!(retired["extension"], extended["extension"]);
            let head = Repository::open(&config.storage_root)
                .unwrap()
                .head()
                .unwrap()
                .target();
            assert_eq!(reconcile(cx, &reopened, config).unchanged, 1);
            assert_eq!(
                Repository::open(&config.storage_root)
                    .unwrap()
                    .head()
                    .unwrap()
                    .target(),
                head
            );

            // Reconstruct from archive alone: the lifecycle state must survive
            // without salvage from the live DB or the retry journal.
            let retired_copy = scratch.join("retired-reconstructed.sqlite3");
            mcp_agent_mail_db::reconstruct::reconstruct_from_archive(
                &retired_copy,
                &config.storage_root,
            )
            .unwrap();
            let reconstructed =
                mcp_agent_mail_db::DbConn::open_file(&retired_copy.to_string_lossy()).unwrap();
            let rows = reconstructed
                .query_sync(
                    "SELECT retired_at, reaper_exempt FROM agents WHERE name = 'BlueLake'",
                    &[],
                )
                .unwrap();
            assert_eq!(rows[0].get_named::<i64>("retired_at").unwrap(), 123_456_789);
            assert_eq!(rows[0].get_named::<i64>("reaper_exempt").unwrap(), 1);
            drop(reconstructed);

            outcome(block_on(mcp_agent_mail_db::queries::set_agent_retired_at(
                cx, &reopened, 101, None,
            )))
            .unwrap();
            assert_eq!(reconcile(cx, &reopened, config).repaired, 1);
            assert!(assert_profile_committed(config)["retired_at"].is_null());

            outcome(block_on(mcp_agent_mail_db::queries::deregister_agent(
                cx,
                &reopened,
                101,
                234_567_891,
            )))
            .unwrap();
            assert_eq!(reconcile(cx, &reopened, config).repaired, 1);
            let deregistered = assert_profile_committed(config);
            assert_eq!(
                deregistered["deregistered_at"],
                timestamp(234_567_891).unwrap()
            );
            assert_eq!(deregistered["contact_policy"], "block_all");
            assert_eq!(deregistered["extension"], extended["extension"]);
            assert_eq!(reconcile(cx, &reopened, config).unchanged, 1);
            let deregistered_copy = scratch.join("deregistered-reconstructed.sqlite3");
            mcp_agent_mail_db::reconstruct::reconstruct_from_archive(
                &deregistered_copy,
                &config.storage_root,
            )
            .unwrap();
            let reconstructed =
                mcp_agent_mail_db::DbConn::open_file(&deregistered_copy.to_string_lossy()).unwrap();
            let rows = reconstructed.query_sync("SELECT d.deregistered_at FROM agent_deregistrations d JOIN agents a ON a.id = d.agent_id WHERE a.name = 'BlueLake'", &[]).unwrap();
            assert_eq!(
                rows[0].get_named::<i64>("deregistered_at").unwrap(),
                234_567_891
            );
            let conn = outcome(block_on(reopened.acquire(cx))).unwrap();
            let rows = conn
                .query_sync("SELECT registration_token FROM agents WHERE id = 101", &[])
                .unwrap();
            assert_eq!(
                rows[0].get_named::<String>("registration_token").unwrap(),
                "never-archive-this-secret"
            );
        });
    }

    #[test]
    fn lost_profile_and_project_files_restore_committed_extensions() {
        with_mailbox(|cx, pool, config, scratch| {
            assert_eq!(reconcile(cx, pool, config).repaired, 1);
            let mut profile = assert_profile_committed(config);
            profile["extension"] = json!({"history": 7});
            crate::write_json(&profile_path(config), &profile, true).unwrap();
            crate::commit_paths_with_retry(
                &config.storage_root,
                config,
                "fixture: committed-only extension",
                &["projects/project/agents/BlueLake/profile.json"],
            )
            .unwrap();
            // Preserve displaced files as evidence instead of deleting them.
            fs::rename(profile_path(config), scratch.join("displaced-profile.json")).unwrap();
            fs::rename(
                config.storage_root.join("projects/project/project.json"),
                scratch.join("displaced-project.json"),
            )
            .unwrap();
            outcome(block_on(mcp_agent_mail_db::queries::set_agent_retired_at(
                cx,
                pool,
                101,
                Some(333_000_001),
            )))
            .unwrap();
            let report = reconcile(cx, pool, config);
            assert_eq!(report.repaired, 1);
            let repaired = assert_profile_committed(config);
            assert_eq!(repaired["extension"], profile["extension"]);
            assert_eq!(repaired["retired_at"], timestamp(333_000_001).unwrap());
            let project = read_artifact(&config.storage_root.join("projects/project/project.json"))
                .unwrap()
                .unwrap();
            assert_eq!(project.value["human_key"], "/project");
        });
    }

    #[test]
    fn malformed_foreign_and_permanent_deregistration_evidence_is_preserved() {
        with_mailbox(|cx, pool, config, _| {
            assert_eq!(reconcile(cx, pool, config).repaired, 1);
            let original = assert_profile_committed(config);
            for conflict in [
                json!({"name": "RedFox"}),
                json!({"name": "BlueLake", "inception_ts": "2026-09-20T00:00:00Z"}),
                json!({"name": "BlueLake", "deregistered_at": "2026-09-20T00:00:00Z"}),
                json!({"name": "BlueLake", "contact_policy": "block_all", "task_description": "[DEREGISTERED 2026-09-20T00:00:00Z] history"}),
            ] {
                crate::write_json(&profile_path(config), &conflict, true).unwrap();
                let before = fs::read(profile_path(config)).unwrap();
                let report = reconcile(cx, pool, config);
                assert_eq!(report.deferred, 1);
                assert_eq!(report.repaired, 0);
                assert_eq!(fs::read(profile_path(config)).unwrap(), before);
            }
            fs::write(profile_path(config), b"{broken profile").unwrap();
            assert_eq!(reconcile(cx, pool, config).deferred, 1);
            assert_eq!(fs::read(profile_path(config)).unwrap(), b"{broken profile");
            crate::write_json(&profile_path(config), &original, true).unwrap();
            let project_path = config.storage_root.join("projects/project/project.json");
            crate::write_json(
                &project_path,
                &json!({"slug": "project", "human_key": "/foreign-project"}),
                true,
            )
            .unwrap();
            let before = fs::read(&project_path).unwrap();
            assert_eq!(reconcile(cx, pool, config).deferred, 1);
            assert_eq!(fs::read(&project_path).unwrap(), before);
            assert_eq!(assert_profile_committed(config), original);
        });
    }

    #[test]
    fn heartbeat_only_changes_do_not_create_archive_commits() {
        with_mailbox(|cx, pool, config, _| {
            assert_eq!(reconcile(cx, pool, config).repaired, 1);
            let before = assert_profile_committed(config);
            let head = Repository::open(&config.storage_root)
                .unwrap()
                .head()
                .unwrap()
                .target();
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE agents SET last_active_ts = 999999999 WHERE id = 101")
                .unwrap();
            drop(conn);
            let report = reconcile(cx, pool, config);
            assert_eq!(report.repaired, 0);
            assert_eq!(report.unchanged, 1);
            assert_eq!(assert_profile_committed(config), before);
            assert_eq!(
                Repository::open(&config.storage_root)
                    .unwrap()
                    .head()
                    .unwrap()
                    .target(),
                head
            );
        });
    }

    #[test]
    fn equivalent_archived_lifecycle_timestamps_preserve_the_live_ledger() {
        with_mailbox(|cx, pool, config, _| {
            outcome(block_on(mcp_agent_mail_db::queries::deregister_agent(
                cx,
                pool,
                101,
                234_000_000,
            )))
            .unwrap();
            assert_eq!(reconcile(cx, pool, config).repaired, 1);
            let mut profile = assert_profile_committed(config);
            profile["inception_ts"] = json!("1970-01-01T00:00:01+00:00");
            profile["registered_ts"] = json!(1_000_000);
            profile["deregistered_at"] = json!("1970-01-01T01:03:54+01:00");
            crate::write_json(&profile_path(config), &profile, true).unwrap();
            crate::commit_paths_with_retry(
                &config.storage_root,
                config,
                "fixture: equivalent timestamps",
                &["projects/project/agents/BlueLake/profile.json"],
            )
            .unwrap();
            let report = reconcile(cx, pool, config);
            assert_eq!(report.repaired, 1);
            assert_eq!(report.deferred, 0);
            assert_eq!(
                assert_profile_committed(config)["deregistered_at"],
                timestamp(234_000_000).unwrap()
            );
        });
    }

    #[test]
    fn finite_rounds_retry_failures_without_starving_later_agents() {
        with_mailbox(|cx, pool, config, scratch| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 102..108 {
                conn.execute_sync("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES(?, 101, ?, 'test', 'test', '', 1000000, 2000000)",
                    &[id.into(), format!("Agent{id}").into()]).unwrap();
            }
            drop(conn);
            let blocked = profile_path(config);
            fs::create_dir_all(&blocked).unwrap();
            let mut cursor = AgentReconcileCursor::default();
            let stop = AtomicBool::new(false);
            let first = reconcile_agent_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(first.deferred, 1);
            assert_eq!(first.repaired, MAX_REPAIRS_PER_BATCH);
            assert!(first.budget_exhausted);
            assert_eq!(cursor.after, 105);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES(108, 101, 'Agent108', 'test', 'test', '', 1000000, 2000000)").unwrap();
            drop(conn);
            let second = reconcile_agent_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(second.repaired, 2);
            assert_eq!(cursor.after, 107);
            // Clearing this fixture's obstruction models repaired disk access.
            fs::rename(&blocked, scratch.join("retained-obstruction")).unwrap();
            let third = reconcile_agent_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(third.repaired, 2);
            assert_eq!(third.unchanged, 6);
            assert_eq!(third.deferred, 0);
            assert_profile_committed(config);
            stop.store(true, Ordering::Release);
            let stopped = reconcile_agent_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(stopped.scanned, 0);
            assert!(stopped.interrupted);
        });
    }

    #[test]
    fn foreign_and_query_only_mailboxes_cannot_authorize_profile_repairs() {
        with_mailbox(|cx, pool, config, scratch| {
            let readonly = DbPool::new_query_only(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let error = reconcile_agent_batch(
                cx,
                &readonly,
                config,
                &mut AgentReconcileCursor::default(),
                &AtomicBool::new(false),
            )
            .unwrap_err();
            assert!(error.contains("query-only snapshots"), "{error}");
            assert!(!profile_path(config).exists());
            let other_path = scratch.join("foreign.sqlite3");
            fs::write(&other_path, b"foreign source is unchanged").unwrap();
            let other = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&other_path),
                ..config.clone()
            };
            let error = reconcile_agent_batch(
                cx,
                pool,
                &other,
                &mut AgentReconcileCursor::default(),
                &AtomicBool::new(false),
            )
            .unwrap_err();
            assert!(
                error.contains("not the configured live database"),
                "{error}"
            );
            assert_eq!(
                fs::read(other_path).unwrap(),
                b"foreign source is unchanged"
            );
            assert!(!profile_path(config).exists());
        });
    }

    #[test]
    fn oversized_profiles_defer_without_archiving_tokens_or_losing_later_work() {
        with_mailbox(|cx, pool, config, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_sync(
                "UPDATE agents SET task_description = ? WHERE id = 101",
                &["x".repeat(MAX_PROFILE_BYTES + 1).into()],
            )
            .unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES(102, 101, 'RedFox', 'test', 'test', '', 1000000, 2000000)").unwrap();
            drop(conn);
            let report = reconcile(cx, pool, config);
            assert_eq!(report.deferred, 1);
            assert_eq!(report.repaired, 1);
            assert!(!profile_path(config).exists());
            assert!(
                config
                    .storage_root
                    .join("projects/project/agents/RedFox/profile.json")
                    .exists()
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_profile_and_parent_never_modify_foreign_artifacts() {
        with_mailbox(|cx, pool, config, scratch| {
            let target = scratch.join("foreign-profile.json");
            fs::write(&target, b"foreign profile stays unchanged").unwrap();
            let path = profile_path(config);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert_eq!(reconcile(cx, pool, config).deferred, 1);
            assert_eq!(
                fs::read(&target).unwrap(),
                b"foreign profile stays unchanged"
            );
            fs::rename(&path, scratch.join("retained-profile-symlink")).unwrap();
            fs::rename(
                path.parent().unwrap(),
                scratch.join("retained-agent-directory"),
            )
            .unwrap();
            std::os::unix::fs::symlink(scratch, path.parent().unwrap()).unwrap();
            assert_eq!(reconcile(cx, pool, config).deferred, 1);
            assert!(!scratch.join("profile.json").exists());
            assert_eq!(
                fs::read(target).unwrap(),
                b"foreign profile stays unchanged"
            );
        });
    }
}
