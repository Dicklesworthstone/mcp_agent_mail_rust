//! Replay reservation closeouts with their original scope and creation cutoff.
//!
//! A queued release is not a standing order against future leases. Resolve
//! only existing identities, intersect path and ID filters, and recheck the
//! original creation cutoff inside the database mutation transaction. Archive
//! publication follows the existing terminal-release path; its result is not
//! a certificate for the entire archive.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use asupersync::Cx;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, pattern_overlap::CompiledPattern};
use mcp_agent_mail_db::{DbPool, FileReservationRow, micros_to_iso, queries};
use mcp_agent_mail_tools::degraded_intents::{self as journal, QueuedReleaseIntentView};
use mcp_agent_mail_tools::tool_util::{resolve_agent, resolve_existing_project};
use serde_json::json;

use super::{MAX_ATTEMPTS, ReplayReport, RoundCursor, db_value, validate_live_pool};

const RELEASE_LOCK: &str = ".release_file_reservations.jsonl.lock";

pub(super) async fn replay_batch(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut RoundCursor,
    shutdown: &AtomicBool,
    intents: &[QueuedReleaseIntentView],
) -> Result<ReplayReport, String> {
    let mut report = ReplayReport::default();
    if shutdown.load(Ordering::Acquire) {
        report.interrupted = true;
        return Ok(report);
    }
    validate_live_pool(cx, pool, config).await?;
    let keys: Vec<_> = intents
        .iter()
        .map(|intent| (intent.created_ts, intent.content_sha256.clone()))
        .collect();
    let candidates = cursor.candidates(&keys);
    report.more = candidates.len() > MAX_ATTEMPTS;
    let ctx = McpContext::new(cx.clone(), 0);
    for index in candidates.into_iter().take(MAX_ATTEMPTS) {
        if shutdown.load(Ordering::Acquire) || ctx.checkpoint().is_err() {
            report.interrupted = true;
            report.more = true;
            break;
        }
        let intent = &intents[index];
        cursor.after = Some(keys[index].clone());
        report.attempted += 1;
        match apply_release(&ctx, pool, config, intent).await {
            Ok(released) => {
                report.applied += 1;
                report.rows_released += released;
                match append_completion(config, intent, released) {
                    Ok(()) => report.completed += 1,
                    Err(error) => {
                        report.deferred += 1;
                        tracing::warn!(intent_id = %intent.intent_id, %error,
                            "release applied but completion receipt unavailable; replay retained");
                    }
                }
            }
            Err(error) => {
                report.deferred += 1;
                tracing::warn!(intent_id = %intent.intent_id, %error,
                    "durable reservation release remains queued");
            }
        }
    }
    Ok(report)
}

fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" || input.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            let root = PathBuf::from(home);
            return input.strip_prefix("~/").map_or(root.clone(), |rest| root.join(rest));
        }
    }
    PathBuf::from(input)
}

fn path_looks_absolute(input: &str) -> bool {
    if input.starts_with("//") {
        return false;
    }
    if Path::new(input).is_absolute() || input.starts_with("~/") || input == "~" {
        return true;
    }
    let bytes = input.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn normalize_parts(input: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    for part in input.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => { parts.pop()?; }
            other => parts.push(other),
        }
    }
    Some(parts)
}

/// Apply the tool's lexical project-relative normalization, not filesystem
/// canonicalization (a queued glob need not name an existing file).
fn normalize_paths(root: &str, paths: Option<&[String]>) -> Result<Option<Vec<String>>, String> {
    let Some(paths) = paths else { return Ok(None); };
    let root = expand_tilde(root).to_string_lossy().into_owned();
    let root_parts = normalize_parts(&root).ok_or_else(|| "invalid release project root".to_string())?;
    let mut normalized = Vec::with_capacity(paths.len());
    for path in paths {
        if path.contains('\0') {
            return Err("release path contains a NUL byte".to_string());
        }
        let expanded = expand_tilde(path).to_string_lossy().into_owned();
        let parts = normalize_parts(&expanded)
            .ok_or_else(|| "release path escapes the project".to_string())?;
        let relative = if path_looks_absolute(&expanded) {
            if parts.len() < root_parts.len() || !parts.iter().zip(&root_parts).all(|(part, root)| {
                if cfg!(windows) { part.eq_ignore_ascii_case(root) } else { part == root }
            }) {
                return Err("release path is outside the project".to_string());
            }
            parts[root_parts.len()..].join("/")
        } else {
            parts.join("/")
        };
        let compiled = CompiledPattern::cached(&relative);
        if relative.is_empty() || relative.contains("..")
            || (compiled.is_glob() && !compiled.is_matchable())
        {
            return Err("invalid queued release pattern; scope preserved".to_string());
        }
        normalized.push(relative);
    }
    Ok(Some(normalized))
}

fn matches_scope(row: &FileReservationRow, agent_id: i64, intent: &QueuedReleaseIntentView,
    paths: Option<&[String]>) -> bool {
    if row.agent_id != agent_id || row.released_ts.is_some_and(|ts| ts > 0)
        || row.created_ts > intent.created_ts
    {
        return false;
    }
    if let Some(ids) = &intent.file_reservation_ids
        && row.id.is_none_or(|id| !ids.contains(&id))
    {
        return false;
    }
    paths.is_none_or(|patterns| {
        let row_pattern = CompiledPattern::cached(&row.path_pattern);
        patterns.iter().any(|pattern| row.path_pattern == *pattern
            || CompiledPattern::cached(pattern).overlaps(&row_pattern))
    })
}

async fn apply_release(ctx: &McpContext, pool: &DbPool, config: &Config,
    intent: &QueuedReleaseIntentView) -> Result<usize, String> {
    if intent.created_ts <= 0 {
        return Err("queued release has no positive creation cutoff".to_string());
    }
    let project = resolve_existing_project(ctx, pool, &intent.project_key)
        .await.map_err(|error| error.to_string())?;
    let project_id = project.id.filter(|id| *id > 0)
        .ok_or_else(|| "release project has no positive identity".to_string())?;
    let paths = normalize_paths(&project.human_key, intent.paths.as_deref())?;
    let agent = resolve_agent(ctx, pool, project_id, &intent.agent_name,
        &project.slug, &project.human_key).await.map_err(|error| error.to_string())?;
    let agent_id = agent.id.filter(|id| *id > 0)
        .ok_or_else(|| "release agent has no positive identity".to_string())?;
    let ids = if paths.is_some() || intent.file_reservation_ids.is_some() {
        let rows = db_value(queries::list_unreleased_file_reservations(ctx.cx(), pool, project_id).await)?;
        Some(rows.iter().filter(|row| matches_scope(row, agent_id, intent, paths.as_deref()))
            .filter_map(|row| row.id).collect::<Vec<_>>())
    } else {
        None
    };
    // Some(empty) stays an empty selection, never an unrestricted release.
    // Ownership and the original cutoff are enforced again by the transaction.
    let released = db_value(queries::release_reservations_with_created_cutoff(
        ctx.cx(), pool, project_id, agent_id, None, ids.as_deref(), Some(intent.created_ts),
    ).await)?;
    if !released.is_empty() {
        let generation = db_value(queries::db_generation_id(ctx.cx(), pool).await).ok().flatten();
        let reservations = released.iter().map(|row| {
            let mut value = json!({
                "id": row.id.unwrap_or(0), "project": project.human_key, "agent": agent.name,
                "path_pattern": row.path_pattern, "exclusive": row.exclusive != 0,
                "reason": row.reason, "created_ts": micros_to_iso(row.created_ts),
                "expires_ts": micros_to_iso(row.expires_ts),
                "released_ts": row.released_ts.map(micros_to_iso),
            });
            if let Some(generation) = generation.as_ref().filter(|value| !value.is_empty()) {
                value["db_generation"] = json!(generation);
            }
            value
        }).collect();
        let op = mcp_agent_mail_storage::WriteOp::FileReservation {
            project_slug: project.slug, config: config.clone(), reservations,
        };
        match mcp_agent_mail_storage::write_op_sync_direct(&op) {
            mcp_agent_mail_storage::DirectArchiveWrite::Written => {}
            mcp_agent_mail_storage::DirectArchiveWrite::SkippedDiskCritical => {
                tracing::warn!("release applied; disk pressure defers archive convergence");
            }
            mcp_agent_mail_storage::DirectArchiveWrite::Failed(error) => {
                // Only terminal artifacts are queued here. A later retry cannot
                // resurrect an ACTIVE grant or act on the agent's future leases.
                let queued = match mcp_agent_mail_storage::archive_backlog_push(op) {
                    mcp_agent_mail_storage::ArchiveBacklogPush::Queued => "durable",
                    mcp_agent_mail_storage::ArchiveBacklogPush::QueuedEphemeral => "ephemeral",
                    mcp_agent_mail_storage::ArchiveBacklogPush::Dropped => "full",
                };
                tracing::warn!(%error, queued, "release applied; archive retry requested");
            }
        }
    }
    Ok(released.len())
}

fn append_completion(config: &Config, intent: &QueuedReleaseIntentView, released: usize)
    -> std::io::Result<()> {
    let mut record = json!({
        "schema_version": 1, "kind": journal::RELEASE_INTENT_REPLAY_KIND,
        "intent_id": intent.intent_id, "intent_content_sha256": intent.content_sha256,
        "replayed_ts": mcp_agent_mail_db::now_micros(),
        "status": journal::REPLAY_STATUS_REPLAYED, "released": released, "error_detail": null,
    });
    record["content_sha256"] = json!(journal::hash_json_value(&record));
    journal::append_jsonl(config, journal::RELEASE_INTENT_LOG_FILE, RELEASE_LOCK, &record)
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;

    fn queue(config: &Config, created_ts: i64, paths: Option<Vec<String>>, ids: Option<Vec<i64>>) {
        let mut payload = json!({
            "schema_version": 1, "kind": journal::RELEASE_INTENT_KIND,
            "created_ts": created_ts, "project_key": "/replay", "agent_name": "BlueLake",
            "paths": paths, "file_reservation_ids": ids,
            "failure": {"stage": "test", "error_detail": "database unavailable"},
        });
        let hash = journal::hash_json_value(&payload);
        payload["intent_id"] = json!(&hash[..16]);
        payload["content_sha256"] = json!(hash);
        journal::append_jsonl(config, journal::RELEASE_INTENT_LOG_FILE, RELEASE_LOCK, &payload).unwrap();
    }

    #[test]
    fn release_path_normalization_preserves_empty_scope_and_rejects_escape() {
        assert_eq!(normalize_paths("/replay", None).unwrap(), None);
        assert_eq!(normalize_paths("/replay", Some(&[])).unwrap(), Some(vec![]));
        assert_eq!(normalize_paths("/replay", Some(&[
            "/replay/src/./*.rs".into(), "src\\lib.rs".into(), "docs/../src/a.rs".into(),
        ])).unwrap(), Some(vec!["src/*.rs".into(), "src/lib.rs".into(), "src/a.rs".into()]));
        for path in ["../outside", "/outside/a", "/replayer/a", "/replay", "src/[", "a\0b"] {
            assert!(normalize_paths("/replay", Some(&[path.into()])).is_err(), "{path:?}");
        }
    }

    #[test]
    fn release_completion_round_trips_and_unknown_schema_is_not_empty() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config { storage_root: temp.path().to_path_buf(), ..Config::default() };
        queue(&config, 10, None, None);
        let intents = journal::read_queued_release_intents(&config).unwrap();
        append_completion(&config, &intents[0], 2).unwrap();
        assert!(journal::read_queued_release_intents(&config).unwrap().is_empty());
        journal::append_jsonl(&config, journal::RELEASE_INTENT_LOG_FILE, RELEASE_LOCK,
            &json!({"schema_version": 2, "kind": journal::RELEASE_INTENT_KIND})).unwrap();
        assert!(journal::read_queued_release_intents(&config).is_err());
    }

    #[test]
    fn replay_releases_old_scope_not_future_or_foreign_leases_and_materializes_archive() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            database_url: format!("sqlite://{}", temp.path().join("mail.sqlite3").display()),
            ..Config::default()
        };
        let mut selected = super::super::pool_config(&config);
        selected.run_migrations = true;
        let pool = DbPool::new(&selected).unwrap();
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let cx = Cx::current().unwrap();
            let project = queries::ensure_project(&cx, &pool, "/replay").await.into_result().unwrap();
            let project_id = project.id.unwrap();
            let owner = queries::register_agent(&cx, &pool, project_id, "BlueLake", "codex-cli",
                "test", None, None, None).await.into_result().unwrap().id.unwrap();
            let other = queries::register_agent(&cx, &pool, project_id, "GreenStone", "codex-cli",
                "test", None, None, None).await.into_result().unwrap().id.unwrap();
            let old = queries::create_file_reservations(&cx, &pool, project_id, owner,
                &["src/old.rs", "docs/keep.md"], 3600, true, "before outage").await.into_result().unwrap();
            let fresh = queries::create_file_reservations(&cx, &pool, project_id, owner,
                &["src/new.rs"], 3600, true, "after intent").await.into_result().unwrap();
            let foreign = queries::create_file_reservations(&cx, &pool, project_id, other,
                &["src/other.rs"], 3600, true, "other owner").await.into_result().unwrap();
            let old_id = old.iter().find(|row| row.path_pattern == "src/old.rs").unwrap().id.unwrap();
            let doc_id = old.iter().find(|row| row.path_pattern == "docs/keep.md").unwrap().id.unwrap();
            let fresh_id = fresh[0].id.unwrap();
            let other_id = foreign[0].id.unwrap();
            let cutoff = mcp_agent_mail_db::now_micros();
            let conn = pool.acquire(&cx).await.into_result().unwrap();
            conn.execute_raw(&format!("UPDATE file_reservations SET created_ts = {}", cutoff - 1)).unwrap();
            conn.execute_raw(&format!("UPDATE file_reservations SET created_ts = {} WHERE id = {fresh_id}", cutoff + 1)).unwrap();
            drop(conn);
            let archive = mcp_agent_mail_storage::ensure_archive(&config, &project.slug).unwrap();
            queue(&config, cutoff, Some(vec!["/replay/src/*.rs".into()]),
                Some(vec![old_id, doc_id, fresh_id, other_id]));
            let intents = journal::read_queued_release_intents(&config).unwrap();
            let stop = AtomicBool::new(false);
            let report = replay_batch(&cx, &pool, &config, &mut RoundCursor::default(), &stop, &intents).await.unwrap();
            assert_eq!((report.attempted, report.completed, report.rows_released, report.deferred), (1, 1, 1, 0));
            assert!(journal::read_queued_release_intents(&config).unwrap().is_empty());
            let rows = queries::get_reservations_by_ids(&cx, &pool, &[old_id, doc_id, fresh_id, other_id]).await.into_result().unwrap();
            for row in &rows {
                assert_eq!(row.released_ts.is_some_and(|ts| ts > 0), row.id == Some(old_id));
            }
            let generation = queries::db_generation_id(&cx, &pool).await.into_result().unwrap();
            let file = match generation.filter(|value| !value.is_empty()) {
                Some(generation) => format!("id-{old_id}-g{generation}.json"),
                None => format!("id-{old_id}.json"),
            };
            let artifact: serde_json::Value = serde_json::from_slice(
                &std::fs::read(archive.root.join("file_reservations").join(file)).unwrap()).unwrap();
            assert_eq!(artifact["agent"], "BlueLake");
            assert_eq!(artifact["path_pattern"], "src/old.rs");
            assert!(artifact["released_ts"].is_string());
            // A stale snapshot cannot act on the new lease, even on retry.
            let repeated = replay_batch(&cx, &pool, &config, &mut RoundCursor::default(), &stop, &intents).await.unwrap();
            assert_eq!(repeated.rows_released, 0);
            // Explicit empty IDs and explicit empty paths must never mean all.
            for (paths, ids) in [(Some(vec![]), None), (None, Some(vec![]))] {
                queue(&config, cutoff, paths, ids);
                let queued = journal::read_queued_release_intents(&config).unwrap();
                let empty = replay_batch(&cx, &pool, &config, &mut RoundCursor::default(), &stop, &queued).await.unwrap();
                assert_eq!(empty.rows_released, 0);
                assert!(journal::read_queued_release_intents(&config).unwrap().is_empty());
            }
            // Unfiltered replay still applies the original transaction cutoff.
            queue(&config, cutoff, None, None);
            let queued = journal::read_queued_release_intents(&config).unwrap();
            let all = replay_batch(&cx, &pool, &config, &mut RoundCursor::default(), &stop, &queued).await.unwrap();
            assert_eq!(all.rows_released, 1); // only docs/keep.md
            let remaining = queries::get_reservations_by_ids(&cx, &pool, &[fresh_id, other_id]).await.into_result().unwrap();
            assert!(remaining.iter().all(|row| row.released_ts.is_none_or(|ts| ts <= 0)));

            // An invalid release never broadens its scope or rewrites the
            // durable journal with another failure on each maintenance tick.
            queue(&config, cutoff, Some(vec!["../escape".into()]), None);
            let invalid = journal::read_queued_release_intents(&config).unwrap();
            let log = journal::log_path(&config, journal::RELEASE_INTENT_LOG_FILE);
            let before = std::fs::read(&log).unwrap();
            let mut cursor = RoundCursor::default();
            for _ in 0..2 {
                let rejected = replay_batch(&cx, &pool, &config, &mut cursor, &stop, &invalid).await.unwrap();
                assert_eq!((rejected.attempted, rejected.completed, rejected.deferred), (1, 0, 1));
                assert_eq!(std::fs::read(&log).unwrap(), before);
            }
            let remaining = queries::get_reservations_by_ids(&cx, &pool, &[fresh_id, other_id]).await.into_result().unwrap();
            assert!(remaining.iter().all(|row| row.released_ts.is_none_or(|ts| ts <= 0)));
            mcp_agent_mail_storage::flush_async_commits();
        });
    }
}
