//! Replay reservation closeouts with their original scope and creation cutoff.
//!
//! A queued release is not a standing order against future leases. Resolve
//! only existing identities, intersect path and ID filters, and recheck the
//! original creation cutoff inside the database mutation transaction. Archive
//! repair re-reads live terminal authority and writes only a generation-stamped
//! stable artifact. Captured release payloads are never queued: a digest alias
//! may already name a newer lease. Completion receipts certify the DB operation,
//! not the entire archive. Bulk intents advance one bounded page per pass and
//! remain queued until their complete, finite scope has been examined.
//! All database pages and completion receipts in a pass precede archive work.
//! A failed or blocked archive follow-up cannot strand that pass's closeouts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use asupersync::Cx;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, pattern_overlap::CompiledPattern};
use mcp_agent_mail_db::{DbPool, FileReservationRow, queries};
use mcp_agent_mail_storage::recovery::reservation_reconcile::reconcile_released_reservation;
use mcp_agent_mail_tools::degraded_intents::{self as journal, QueuedReleaseIntentView};
use mcp_agent_mail_tools::tool_util::{resolve_agent, resolve_existing_project};
use serde_json::json;

use super::{MAX_ATTEMPTS, ReplayReport, db_value, validate_live_pool};

mod paging;
pub(super) use paging::Cursor;

const RELEASE_LOCK: &str = ".release_file_reservations.jsonl.lock";
const MAX_ARCHIVE_REPAIRS_PER_BATCH: usize = 4;

struct AppliedPage {
    released: Vec<FileReservationRow>,
    generation: Option<String>,
    complete: bool,
}

/// Disposable identity hints for this pass, never a persistent payload queue.
/// At most four rows survive the mutation phase. The release ledger remains
/// authoritative if the worker exits before, or during, archive follow-up.
#[derive(Default)]
struct ArchiveFollowups {
    groups: Vec<(Vec<FileReservationRow>, String)>,
    retained: usize,
    unversioned: usize,
}

impl ArchiveFollowups {
    fn remember(&mut self, page: AppliedPage) {
        let Some(generation) = page.generation.filter(|value| !value.is_empty()) else {
            self.unversioned += page.released.len();
            return;
        };
        let remaining = MAX_ARCHIVE_REPAIRS_PER_BATCH.saturating_sub(self.retained);
        let rows: Vec<_> = page.released.into_iter().take(remaining).collect();
        if !rows.is_empty() {
            self.retained += rows.len();
            self.groups.push((rows, generation));
        }
    }

    fn run(self, cx: &Cx, pool: &DbPool, config: &Config, shutdown: &AtomicBool) {
        if self.unversioned > 0 {
            tracing::warn!(released = self.unversioned,
                "release applied; missing generation defers archive repair without a legacy write");
        }
        let mut budget = MAX_ARCHIVE_REPAIRS_PER_BATCH;
        for (rows, generation) in self.groups {
            if shutdown.load(Ordering::Acquire) || cx.checkpoint().is_err() {
                break;
            }
            // Admission is reacquired by storage and the original generation
            // is checked there. Dropping replay's lease is not authority to
            // publish a release into a replacement mailbox generation.
            reconcile_release_archive(cx, pool, config, &rows, &Ok(Some(generation)), &mut budget);
        }
    }
}

#[cfg(test)]
std::thread_local! {
    // Fault injection at the real receipt/archive boundary, after the source
    // lease drops. Database mutations and journal publication remain real.
    static BEFORE_ARCHIVE_FOLLOWUPS: std::cell::RefCell<Option<Box<dyn FnOnce(usize)>>> =
        const { std::cell::RefCell::new(None) };
}

pub(super) async fn replay_batch(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut Cursor,
    shutdown: &AtomicBool,
    intents: &[QueuedReleaseIntentView],
) -> Result<ReplayReport, String> {
    let mut report = ReplayReport::default();
    if shutdown.load(Ordering::Acquire) || cx.checkpoint().is_err() {
        report.interrupted = true;
        return Ok(report);
    }
    // This path deliberately bypasses public tool handlers and their WriteDbPool
    // lease. Retain the same promotion exclusion through identity resolution,
    // page mutation and completion receipt publication. Archive repair has
    // its own admission and must not precede any closeout in this pass.
    // A retryable closeout must not wait for promotion, including a failed
    // drain whose owner still excludes writers. Refuse before source access
    // or any round/page changes; the supervisor can retry the durable journal.
    let write_activity = mcp_agent_mail_db::write_barrier::try_begin_write_activity()
        .ok_or("durable release replay deferred: recovery promotion or admission contention")?;
    validate_live_pool(cx, pool, config).await?;
    let keys: Vec<_> = intents
        .iter()
        .map(|intent| (intent.created_ts, intent.content_sha256.clone()))
        .collect();
    cursor.prepare(pool.sqlite_identity_key(), &keys)?;
    let candidates = cursor.round.candidates(&keys);
    report.more = candidates.len() > MAX_ATTEMPTS;
    let ctx = McpContext::new(cx.clone(), 0);
    let mut followups = ArchiveFollowups::default();
    for index in candidates.into_iter().take(MAX_ATTEMPTS) {
        if shutdown.load(Ordering::Acquire) || ctx.checkpoint().is_err() {
            report.interrupted = true;
            report.more = true;
            break;
        }
        let intent = &intents[index];
        let key = &keys[index];
        cursor.round.after = Some(key.clone());
        report.attempted += 1;
        let position = cursor.pages.entry(key.clone()).or_default();
        match apply_release(&ctx, pool, intent, position).await {
            Ok(page) => {
                // Applied counts successful pages, including a sparse page
                // that advanced without finding a matching path. It does not
                // imply that the whole intent has completed.
                report.applied += 1;
                report.rows_released += page.released.len();
                if !page.complete {
                    report.more = true;
                } else {
                    match append_completion(config, intent, position.released) {
                        Ok(()) => {
                            report.completed += 1;
                            cursor.pages.remove(key);
                        }
                        Err(error) => {
                            report.deferred += 1;
                            tracing::warn!(intent_id = %intent.intent_id, %error,
                                "release applied but completion receipt unavailable; replay retained");
                        }
                    }
                }
                followups.remember(page);
            }
            Err(error) => {
                report.deferred += 1;
                tracing::warn!(intent_id = %intent.intent_id, %error,
                    "durable reservation release remains queued");
            }
        }
    }
    report.more |= !cursor.pages.is_empty();
    drop(write_activity);
    // Finish the bounded batch's database work and terminal receipts before
    // waiting for any archive I/O. An interrupted follow-up is repaired from
    // the release ledger, even if the intent has already left the journal.
    #[cfg(test)]
    BEFORE_ARCHIVE_FOLLOWUPS.with(|slot| {
        let hook = slot.borrow_mut().take();
        if let Some(hook) = hook {
            hook(followups.retained);
        }
    });
    followups.run(cx, pool, config, shutdown);
    report.interrupted |= shutdown.load(Ordering::Acquire) || cx.checkpoint().is_err();
    Ok(report)
}

fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" || input.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            let root = PathBuf::from(home);
            return input
                .strip_prefix("~/")
                .map_or(root.clone(), |rest| root.join(rest));
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
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn normalize_parts(input: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    for part in input.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts)
}

/// Apply the tool's lexical project-relative normalization, not filesystem
/// canonicalization (a queued glob need not name an existing file).
fn normalize_paths(root: &str, paths: Option<&[String]>) -> Result<Option<Vec<String>>, String> {
    let Some(paths) = paths else {
        return Ok(None);
    };
    let root = expand_tilde(root).to_string_lossy().into_owned();
    let root_parts =
        normalize_parts(&root).ok_or_else(|| "invalid release project root".to_string())?;
    let mut normalized = Vec::with_capacity(paths.len());
    for path in paths {
        if path.contains('\0') {
            return Err("release path contains a NUL byte".to_string());
        }
        let expanded = expand_tilde(path).to_string_lossy().into_owned();
        let parts = normalize_parts(&expanded)
            .ok_or_else(|| "release path escapes the project".to_string())?;
        let relative = if path_looks_absolute(&expanded) {
            if parts.len() < root_parts.len()
                || !parts.iter().zip(&root_parts).all(|(part, root)| {
                    if cfg!(windows) {
                        part.eq_ignore_ascii_case(root)
                    } else {
                        part == root
                    }
                })
            {
                return Err("release path is outside the project".to_string());
            }
            parts[root_parts.len()..].join("/")
        } else {
            parts.join("/")
        };
        let compiled = CompiledPattern::cached(&relative);
        if relative.is_empty()
            || relative.contains("..")
            || (compiled.is_glob() && !compiled.is_matchable())
        {
            return Err("invalid queued release pattern; scope preserved".to_string());
        }
        normalized.push(relative);
    }
    Ok(Some(normalized))
}

fn matches_scope(
    row: &FileReservationRow,
    agent_id: i64,
    intent: &QueuedReleaseIntentView,
    paths: Option<&[String]>,
) -> bool {
    if row.agent_id != agent_id
        || row.released_ts.is_some_and(|ts| ts > 0)
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
        patterns.iter().any(|pattern| {
            row.path_pattern == *pattern || CompiledPattern::cached(pattern).overlaps(&row_pattern)
        })
    })
}

async fn apply_release(
    ctx: &McpContext,
    pool: &DbPool,
    intent: &QueuedReleaseIntentView,
    position: &mut paging::Position,
) -> Result<AppliedPage, String> {
    if intent.created_ts <= 0 {
        return Err("queued release has no positive creation cutoff".to_string());
    }
    let project = resolve_existing_project(ctx, pool, &intent.project_key)
        .await
        .map_err(|error| error.to_string())?;
    let project_id = project
        .id
        .filter(|id| *id > 0)
        .ok_or_else(|| "release project has no positive identity".to_string())?;
    let paths = normalize_paths(&project.human_key, intent.paths.as_deref())?;
    let agent = resolve_agent(
        ctx,
        pool,
        project_id,
        &intent.agent_name,
        &project.slug,
        &project.human_key,
    )
    .await
    .map_err(|error| error.to_string())?;
    let agent_id = agent
        .id
        .filter(|id| *id > 0)
        .ok_or_else(|| "release agent has no positive identity".to_string())?;
    // An explicit empty filter is complete without scanning any lease rows.
    if paths.as_ref().is_some_and(Vec::is_empty)
        || intent
            .file_reservation_ids
            .as_ref()
            .is_some_and(Vec::is_empty)
    {
        return Ok(AppliedPage {
            released: Vec::new(),
            generation: None,
            complete: true,
        });
    }
    // A failed authority query cannot advance a multi-pass scan. A legitimately
    // unseeded generation is allowed, but any later identity/generation change
    // restarts the finite scan rather than carrying a numeric cursor across it.
    let generation = db_value(queries::db_generation_id(ctx.cx(), pool).await)?;
    position.bind(project_id, agent_id, generation.clone());
    let page = paging::select(
        ctx.cx(),
        pool,
        intent.created_ts,
        position,
        intent.file_reservation_ids.as_deref(),
    )
    .await?;
    let rows = if page.ids.is_empty() {
        Vec::new()
    } else {
        db_value(queries::get_reservations_by_ids(ctx.cx(), pool, &page.ids).await)?
    };
    if rows.len() > paging::PAGE_SIZE {
        return Err("release page payload exceeded its row limit".to_string());
    }
    let ids: Vec<_> = rows
        .iter()
        .filter(|row| row.project_id == project_id)
        .filter(|row| matches_scope(row, agent_id, intent, paths.as_deref()))
        .filter_map(|row| row.id.filter(|id| page.ids.contains(id)))
        .collect();
    ctx.checkpoint().map_err(|error| error.to_string())?;
    // Every transaction receives explicit, page-bounded IDs. The query repeats
    // ownership/cutoff checks, and an empty selection never means release-all.
    let released = if ids.is_empty() {
        Vec::new()
    } else {
        db_value(
            queries::release_reservations_with_created_cutoff(
                ctx.cx(),
                pool,
                project_id,
                agent_id,
                None,
                Some(&ids),
                Some(intent.created_ts),
            )
            .await,
        )?
    };
    position.applied(&page, released.len());
    Ok(AppliedPage {
        released,
        generation,
        complete: page.complete,
    })
}

/// Repair at most the remaining per-pass budget; failures consume it too.
/// The release ledger, not a queued snapshot of these rows, is the retry source.
fn reconcile_release_archive(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    released: &[FileReservationRow],
    captured_generation: &Result<Option<String>, String>,
    budget: &mut usize,
) -> usize {
    if released.is_empty() {
        return 0;
    }
    let generation = match captured_generation {
        Ok(Some(generation)) if !generation.is_empty() => generation.as_str(),
        Ok(_) => {
            tracing::warn!(
                released = released.len(),
                "release applied; missing generation defers archive repair without a legacy write"
            );
            return 0;
        }
        Err(error) => {
            tracing::warn!(%error, released = released.len(),
                "release applied; generation lookup failed, archive repair deferred");
            return 0;
        }
    };
    let attempted = released.len().min(*budget);
    *budget -= attempted;
    // The storage repair blocks on the pool internally. Replay runs inside the
    // worker's `block_on`, where a nested one panics and ends the worker, so
    // the repairs run on their own thread, as blocking dispatch does.
    let repairs = &released[..attempted];
    let joined = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("release-archive-repair".into())
            .stack_size(mcp_agent_mail_core::worker_stack_size())
            .spawn_scoped(scope, || {
                for row in repairs {
                    match reconcile_released_reservation(cx, pool, config, row, generation) {
                        Ok(repaired) => {
                            tracing::debug!(reservation_id = ?row.id, repaired,
                                "replayed release stable artifact verified against the live mailbox");
                        }
                        Err(error) => {
                            tracing::warn!(reservation_id = ?row.id, %error,
                                "release applied; archive evidence preserved for live-state reconciliation");
                        }
                    }
                }
            })
            .map(std::thread::ScopedJoinHandle::join)
    });
    match joined {
        Ok(Ok(())) => {}
        Ok(Err(_)) => tracing::warn!(
            attempted,
            "release archive repair panicked; history reconciliation retains the work"
        ),
        Err(error) => tracing::warn!(
            %error, attempted,
            "release archive repair thread unavailable; history reconciliation retains the work"
        ),
    }
    if attempted < released.len() {
        tracing::debug!(
            remaining = released.len() - attempted,
            "release archive repair budget exhausted; history reconciliation retains the remaining work"
        );
    }
    attempted
}

pub(super) fn append_completion(
    config: &Config,
    intent: &QueuedReleaseIntentView,
    released: usize,
) -> std::io::Result<()> {
    let mut record = json!({
        "schema_version": 1, "kind": journal::RELEASE_INTENT_REPLAY_KIND,
        "intent_id": intent.intent_id, "intent_content_sha256": intent.content_sha256,
        "replayed_ts": mcp_agent_mail_db::now_micros(),
        "status": journal::REPLAY_STATUS_REPLAYED, "released": released, "error_detail": null,
    });
    record["content_sha256"] = json!(journal::hash_json_value(&record));
    journal::append_jsonl(
        config,
        journal::RELEASE_INTENT_LOG_FILE,
        RELEASE_LOCK,
        &record,
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use mcp_agent_mail_db::micros_to_iso;

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
        journal::append_jsonl(
            config,
            journal::RELEASE_INTENT_LOG_FILE,
            RELEASE_LOCK,
            &payload,
        )
        .unwrap();
    }

    #[test]
    fn release_path_normalization_preserves_empty_scope_and_rejects_escape() {
        assert_eq!(normalize_paths("/replay", None).unwrap(), None);
        assert_eq!(normalize_paths("/replay", Some(&[])).unwrap(), Some(vec![]));
        assert_eq!(
            normalize_paths(
                "/replay",
                Some(&[
                    "/replay/src/./*.rs".into(),
                    "src\\lib.rs".into(),
                    "docs/../src/a.rs".into(),
                ])
            )
            .unwrap(),
            Some(vec![
                "src/*.rs".into(),
                "src/lib.rs".into(),
                "src/a.rs".into()
            ])
        );
        for path in [
            "../outside",
            "/outside/a",
            "/replayer/a",
            "/replay",
            "src/[",
            "a\0b",
        ] {
            assert!(
                normalize_paths("/replay", Some(&[path.into()])).is_err(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn release_completion_round_trips_and_unknown_schema_is_not_empty() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        queue(&config, 10, None, None);
        let intents = journal::read_queued_release_intents(&config).unwrap();
        append_completion(&config, &intents[0], 2).unwrap();
        assert_eq!(
            journal::read_queued_release_intents(&config).unwrap(),
            [] as [QueuedReleaseIntentView; 0]
        );
        journal::append_jsonl(
            &config,
            journal::RELEASE_INTENT_LOG_FILE,
            RELEASE_LOCK,
            &json!({"schema_version": 2, "kind": journal::RELEASE_INTENT_KIND}),
        )
        .unwrap();
        assert!(journal::read_queued_release_intents(&config).is_err());
    }

    #[test]
    fn replay_releases_old_scope_not_future_or_foreign_leases_and_materializes_archive() {
        if isolated_completion_test() {
            return;
        }
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
            let project = queries::ensure_project(&cx, &pool, "/replay")
                .await
                .into_result()
                .unwrap();
            let project_id = project.id.unwrap();
            let owner = queries::register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "test",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .unwrap()
            .id
            .unwrap();
            let other = queries::register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "test",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .unwrap()
            .id
            .unwrap();
            let old = queries::create_file_reservations(
                &cx,
                &pool,
                project_id,
                owner,
                &["src/old.rs", "docs/keep.md"],
                3600,
                true,
                "before outage",
            )
            .await
            .into_result()
            .unwrap();
            let fresh = queries::create_file_reservations(
                &cx,
                &pool,
                project_id,
                owner,
                &["src/new.rs"],
                3600,
                true,
                "after intent",
            )
            .await
            .into_result()
            .unwrap();
            let foreign = queries::create_file_reservations(
                &cx,
                &pool,
                project_id,
                other,
                &["src/other.rs"],
                3600,
                true,
                "other owner",
            )
            .await
            .into_result()
            .unwrap();
            let old_id = old
                .iter()
                .find(|row| row.path_pattern == "src/old.rs")
                .unwrap()
                .id
                .unwrap();
            let doc_id = old
                .iter()
                .find(|row| row.path_pattern == "docs/keep.md")
                .unwrap()
                .id
                .unwrap();
            let fresh_id = fresh[0].id.unwrap();
            let other_id = foreign[0].id.unwrap();
            let cutoff = mcp_agent_mail_db::now_micros();
            let conn = pool.acquire(&cx).await.into_result().unwrap();
            conn.execute_raw(&format!(
                "UPDATE file_reservations SET created_ts = {}",
                cutoff - 1
            ))
            .unwrap();
            conn.execute_raw(&format!(
                "UPDATE file_reservations SET created_ts = {} WHERE id = {fresh_id}",
                cutoff + 1
            ))
            .unwrap();
            drop(conn);
            let archive = mcp_agent_mail_storage::ensure_archive(&config, &project.slug).unwrap();
            queue(
                &config,
                cutoff,
                Some(vec!["/replay/src/*.rs".into()]),
                Some(vec![old_id, doc_id, fresh_id, other_id]),
            );
            let intents = journal::read_queued_release_intents(&config).unwrap();
            let stop = AtomicBool::new(false);
            let report = replay_batch(&cx, &pool, &config, &mut Cursor::default(), &stop, &intents)
                .await
                .unwrap();
            assert_eq!(
                (
                    report.attempted,
                    report.completed,
                    report.rows_released,
                    report.deferred
                ),
                (1, 1, 1, 0)
            );
            assert_eq!(
                journal::read_queued_release_intents(&config).unwrap(),
                [] as [QueuedReleaseIntentView; 0]
            );
            let rows =
                queries::get_reservations_by_ids(&cx, &pool, &[old_id, doc_id, fresh_id, other_id])
                    .await
                    .into_result()
                    .unwrap();
            for row in &rows {
                assert_eq!(
                    row.released_ts.is_some_and(|ts| ts > 0),
                    row.id == Some(old_id)
                );
            }
            let generation = queries::db_generation_id(&cx, &pool)
                .await
                .into_result()
                .unwrap();
            let file = generation.filter(|value| !value.is_empty()).map_or_else(
                || format!("id-{old_id}.json"),
                |generation| format!("id-{old_id}-g{generation}.json"),
            );
            let artifact: serde_json::Value = serde_json::from_slice(
                &std::fs::read(archive.root.join("file_reservations").join(file)).unwrap(),
            )
            .unwrap();
            assert_eq!(artifact["agent"], "BlueLake");
            assert_eq!(artifact["path_pattern"], "src/old.rs");
            assert!(artifact["released_ts"].is_string());
            // A stale snapshot cannot act on the new lease, even on retry.
            let repeated =
                replay_batch(&cx, &pool, &config, &mut Cursor::default(), &stop, &intents)
                    .await
                    .unwrap();
            assert_eq!(repeated.rows_released, 0);
            // Explicit empty IDs and explicit empty paths must never mean all.
            for (paths, ids) in [(Some(vec![]), None), (None, Some(vec![]))] {
                queue(&config, cutoff, paths, ids);
                let queued = journal::read_queued_release_intents(&config).unwrap();
                let empty =
                    replay_batch(&cx, &pool, &config, &mut Cursor::default(), &stop, &queued)
                        .await
                        .unwrap();
                assert_eq!(empty.rows_released, 0);
                assert_eq!(
                    journal::read_queued_release_intents(&config).unwrap(),
                    [] as [QueuedReleaseIntentView; 0]
                );
            }
            // Unfiltered replay still applies the original transaction cutoff.
            queue(&config, cutoff, None, None);
            let queued = journal::read_queued_release_intents(&config).unwrap();
            let all = replay_batch(&cx, &pool, &config, &mut Cursor::default(), &stop, &queued)
                .await
                .unwrap();
            assert_eq!(all.rows_released, 1); // only docs/keep.md
            let remaining = queries::get_reservations_by_ids(&cx, &pool, &[fresh_id, other_id])
                .await
                .into_result()
                .unwrap();
            assert!(
                remaining
                    .iter()
                    .all(|row| row.released_ts.is_none_or(|ts| ts <= 0))
            );

            // An invalid release never broadens its scope or rewrites the
            // durable journal with another failure on each maintenance tick.
            queue(&config, cutoff, Some(vec!["../escape".into()]), None);
            let invalid = journal::read_queued_release_intents(&config).unwrap();
            let log = journal::log_path(&config, journal::RELEASE_INTENT_LOG_FILE);
            let before = std::fs::read(&log).unwrap();
            let mut cursor = Cursor::default();
            for _ in 0..2 {
                let rejected = replay_batch(&cx, &pool, &config, &mut cursor, &stop, &invalid)
                    .await
                    .unwrap();
                assert_eq!(
                    (rejected.attempted, rejected.completed, rejected.deferred),
                    (1, 0, 1)
                );
                assert_eq!(std::fs::read(&log).unwrap(), before);
            }
            let remaining = queries::get_reservations_by_ids(&cx, &pool, &[fresh_id, other_id])
                .await
                .into_result()
                .unwrap();
            assert!(
                remaining
                    .iter()
                    .all(|row| row.released_ts.is_none_or(|ts| ts <= 0))
            );
            mcp_agent_mail_storage::flush_async_commits();
        });
    }

    fn with_replay_mailbox(test: impl FnOnce(&Cx, &DbPool, &Config, &str, &str)) {
        if isolated_completion_test() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let config = Config {
                storage_root: temp.path().join("archive"),
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                    &temp.path().join("mail.sqlite3"),
                ),
                ..Config::default()
            };
            std::fs::create_dir_all(&config.storage_root).unwrap();
            let project_root = temp.path().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let project_key = project_root.to_string_lossy().into_owned();
            let mut selected = super::super::pool_config(&config);
            selected.run_migrations = true;
            let pool = DbPool::new(&selected).unwrap();
            let cx = Cx::for_testing();
            let conn = fastmcp_core::block_on(pool.acquire(&cx))
                .into_result()
                .unwrap();
            let escaped_key = project_key.replace('\'', "''");
            conn.execute_raw(&format!("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'replay', '{escaped_key}', 1)")).unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1)").unwrap();
            drop(conn);
            let generation = fastmcp_core::block_on(queries::db_generation_id(&cx, &pool))
                .into_result()
                .unwrap()
                .expect("file mailbox generation");
            test(&cx, &pool, &config, &generation, &project_key);
            mcp_agent_mail_storage::flush_async_commits();
        });
    }

    fn seed_replay_lease(
        cx: &Cx,
        pool: &DbPool,
        id: i64,
        pattern: &str,
        created: i64,
    ) -> FileReservationRow {
        let expires = mcp_agent_mail_db::now_micros() + 3_600_000_000;
        let conn = fastmcp_core::block_on(pool.acquire(cx))
            .into_result()
            .unwrap();
        conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, '{pattern}', 1, 'test lease', {created}, {expires}, NULL)")).unwrap();
        drop(conn);
        fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &[id]))
            .into_result()
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    fn queue_fixture_release(config: &Config, cutoff: i64, id: i64) {
        let mut record = json!({
            "schema_version": 1, "kind": journal::RELEASE_INTENT_KIND,
            "created_ts": cutoff, "project_key": "replay", "agent_name": "BlueLake",
            "paths": null, "file_reservation_ids": [id],
            "failure": {"stage": "test", "error_detail": "database unavailable"},
        });
        let hash = journal::hash_json_value(&record);
        record["intent_id"] = json!(&hash[..16]);
        record["content_sha256"] = json!(hash);
        journal::append_jsonl(
            config,
            journal::RELEASE_INTENT_LOG_FILE,
            RELEASE_LOCK,
            &record,
        )
        .unwrap();
    }

    fn replay_fixture(
        cx: &Cx,
        pool: &DbPool,
        config: &Config,
        intents: &[QueuedReleaseIntentView],
    ) -> ReplayReport {
        fastmcp_core::block_on(replay_batch(
            cx,
            pool,
            config,
            &mut Cursor::default(),
            &AtomicBool::new(false),
            intents,
        ))
        .unwrap()
    }

    fn lease_artifact(
        row: &FileReservationRow,
        generation: &str,
        project_key: &str,
    ) -> serde_json::Value {
        json!({
            "id": row.id, "project": project_key, "agent": "BlueLake",
            "path_pattern": row.path_pattern, "exclusive": row.exclusive != 0,
            "reason": row.reason, "created_ts": micros_to_iso(row.created_ts),
            "expires_ts": micros_to_iso(row.expires_ts),
            "released_ts": row.released_ts.map(micros_to_iso),
            "db_generation": generation,
        })
    }

    #[test]
    fn replay_does_not_overwrite_a_newer_live_lease_digest_alias() {
        with_replay_mailbox(|cx, pool, config, generation, project_key| {
            let cutoff = mcp_agent_mail_db::now_micros();
            seed_replay_lease(cx, pool, 401, "src/shared.rs", cutoff - 1);
            let newer = seed_replay_lease(cx, pool, 402, "src/shared.rs", cutoff + 1);
            let archive = mcp_agent_mail_storage::ensure_archive(config, "replay").unwrap();
            mcp_agent_mail_storage::write_file_reservation_record(
                &archive,
                config,
                &lease_artifact(&newer, generation, project_key),
            )
            .unwrap();
            mcp_agent_mail_storage::flush_async_commits();
            let directory = archive.root.join("file_reservations");
            // sha1("src/shared.rs"), the actual reusable path written above.
            let alias = directory.join("137d3ab6a41fd6fbc01bec4392b65aa81bb8ae90.json");
            let stable = |id| {
                directory.join(
                    mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
                        Some(generation),
                        id,
                    ),
                )
            };
            let alias_before = std::fs::read(&alias).unwrap();
            let newer_before = std::fs::read(stable(402)).unwrap();
            queue_fixture_release(config, cutoff, 401);
            let intents = journal::read_queued_release_intents(config).unwrap();
            let report = replay_fixture(cx, pool, config, &intents);
            assert_eq!(
                (report.completed, report.rows_released, report.deferred),
                (1, 1, 0)
            );
            assert_eq!(
                journal::read_queued_release_intents(config).unwrap(),
                [] as [QueuedReleaseIntentView; 0]
            );
            let rows =
                fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &[401, 402]))
                    .into_result()
                    .unwrap();
            assert!(
                rows.iter()
                    .find(|row| row.id == Some(401))
                    .unwrap()
                    .released_ts
                    .is_some()
            );
            assert!(
                rows.iter()
                    .find(|row| row.id == Some(402))
                    .unwrap()
                    .is_active()
            );
            let released: serde_json::Value =
                serde_json::from_slice(&std::fs::read(stable(401)).unwrap()).unwrap();
            assert_eq!(released["db_generation"], generation);
            assert!(released["released_ts"].is_string());
            assert_eq!(std::fs::read(&alias).unwrap(), alias_before);
            assert_eq!(std::fs::read(stable(402)).unwrap(), newer_before);
            let repeated = replay_fixture(cx, pool, config, &intents);
            assert_eq!(repeated.rows_released, 0);
            assert_eq!(std::fs::read(&alias).unwrap(), alias_before);
        });
    }

    #[test]
    fn replay_completes_db_release_without_overwriting_conflicting_archive_evidence() {
        with_replay_mailbox(|cx, pool, config, generation, project_key| {
            let cutoff = mcp_agent_mail_db::now_micros();
            let old = seed_replay_lease(cx, pool, 401, "src/owned.rs", cutoff - 1);
            let archive = mcp_agent_mail_storage::ensure_archive(config, "replay").unwrap();
            let directory = archive.root.join("file_reservations");
            std::fs::create_dir_all(&directory).unwrap();
            let stable = directory.join(
                mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
                    Some(generation),
                    401,
                ),
            );
            let foreign = lease_artifact(&old, &format!("ff{generation}"), project_key);
            let before = serde_json::to_vec_pretty(&foreign).unwrap();
            std::fs::write(&stable, &before).unwrap();
            queue_fixture_release(config, cutoff, 401);
            let intents = journal::read_queued_release_intents(config).unwrap();
            let report = replay_fixture(cx, pool, config, &intents);
            assert_eq!(
                (report.completed, report.rows_released, report.deferred),
                (1, 1, 0)
            );
            assert_eq!(
                journal::read_queued_release_intents(config).unwrap(),
                [] as [QueuedReleaseIntentView; 0]
            );
            mcp_agent_mail_storage::flush_async_commits();
            assert_eq!(std::fs::read(&stable).unwrap(), before);
            let rows = fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &[401]))
                .into_result()
                .unwrap();
            assert!(rows[0].released_ts.is_some());
        });
    }

    #[test]
    fn missing_generation_never_falls_back_to_an_unstamped_archive_write() {
        with_replay_mailbox(|cx, pool, config, generation, _| {
            let cutoff = mcp_agent_mail_db::now_micros();
            seed_replay_lease(cx, pool, 401, "src/owned.rs", cutoff - 1);
            let released =
                fastmcp_core::block_on(queries::release_reservations_with_created_cutoff(
                    cx,
                    pool,
                    71,
                    81,
                    None,
                    Some(&[401]),
                    Some(cutoff),
                ))
                .into_result()
                .unwrap();
            assert_eq!(released.len(), 1);
            let archive = mcp_agent_mail_storage::ensure_archive(config, "replay").unwrap();
            let directory = archive.root.join("file_reservations");
            std::fs::create_dir_all(&directory).unwrap();
            let legacy = directory.join("id-401.json");
            let before = b"prior generation evidence must not be interpreted as a current lease";
            std::fs::write(&legacy, before).unwrap();
            let stable = directory.join(
                mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
                    Some(generation),
                    401,
                ),
            );
            for unavailable in [
                Ok(None),
                Ok(Some(String::new())),
                Err("generation read failed".to_string()),
            ] {
                let mut budget = MAX_ARCHIVE_REPAIRS_PER_BATCH;
                assert_eq!(
                    reconcile_release_archive(
                        cx,
                        pool,
                        config,
                        &released,
                        &unavailable,
                        &mut budget
                    ),
                    0
                );
                assert_eq!(budget, MAX_ARCHIVE_REPAIRS_PER_BATCH);
                assert_eq!(std::fs::read(&legacy).unwrap(), before);
                assert!(!stable.exists());
            }
        });
    }

    #[test]
    fn replay_archive_budget_is_shared_across_intents_and_failed_publications() {
        for fail_git in [false, true] {
            with_replay_mailbox(|cx, pool, config, generation, _| {
                let cutoff = mcp_agent_mail_db::now_micros();
                let ids: Vec<_> = (401..407).collect();
                for &id in &ids {
                    seed_replay_lease(cx, pool, id, &format!("src/{id}.rs"), cutoff - 1);
                    queue_fixture_release(config, cutoff, id);
                }
                let archive = mcp_agent_mail_storage::ensure_archive(config, "replay").unwrap();
                let mut selected = config.clone();
                if fail_git {
                    selected.git_author_name = "invalid\0author".into();
                }
                let intents = journal::read_queued_release_intents(config).unwrap();
                let report = replay_fixture(cx, pool, &selected, &intents);
                assert_eq!(
                    (
                        report.attempted,
                        report.completed,
                        report.rows_released,
                        report.deferred
                    ),
                    (6, 6, 6, 0)
                );
                assert_eq!(
                    journal::read_queued_release_intents(config).unwrap(),
                    [] as [QueuedReleaseIntentView; 0]
                );
                let rows = fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &ids))
                    .into_result()
                    .unwrap();
                assert!(rows.iter().all(|row| row.released_ts.is_some()));
                let path = |id| {
                    archive.root.join("file_reservations").join(
                        mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
                            Some(generation),
                            id,
                        ),
                    )
                };
                assert_eq!(
                    ids.iter().filter(|&&id| path(id).exists()).count(),
                    MAX_ARCHIVE_REPAIRS_PER_BATCH
                );

                // Remaining or uncommitted rows converge from the release
                // ledger; no captured WriteOp needs to survive this pass.
                let mut cursor = mcp_agent_mail_storage::recovery::reservation_reconcile::ReservationReconcileCursor::default();
                for _ in 0..3 {
                    mcp_agent_mail_storage::recovery::reservation_reconcile::reconcile_reservation_releases(
                        cx, pool, config, &mut cursor, &AtomicBool::new(false),
                    ).unwrap();
                }
                for id in ids {
                    let artifact: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(path(id)).unwrap()).unwrap();
                    assert_eq!(artifact["id"], id);
                    assert_eq!(artifact["db_generation"], generation);
                    assert!(artifact["released_ts"].is_string());
                }
            });
        }
    }

    fn queue_scope(
        config: &Config,
        cutoff: i64,
        agent: &str,
        paths: Option<Vec<String>>,
        ids: Option<Vec<i64>>,
    ) {
        let mut record = json!({
            "schema_version": 1, "kind": journal::RELEASE_INTENT_KIND,
            "created_ts": cutoff, "project_key": "replay", "agent_name": agent,
            "paths": paths, "file_reservation_ids": ids,
            "failure": {"stage": "test", "error_detail": "database unavailable"},
        });
        let hash = journal::hash_json_value(&record);
        record["intent_id"] = json!(&hash[..16]);
        record["content_sha256"] = json!(hash);
        journal::append_jsonl(
            config,
            journal::RELEASE_INTENT_LOG_FILE,
            RELEASE_LOCK,
            &record,
        )
        .unwrap();
    }

    fn page_pass(cx: &Cx, pool: &DbPool, config: &Config, cursor: &mut Cursor) -> ReplayReport {
        let intents = journal::read_queued_release_intents(config).unwrap();
        fastmcp_core::block_on(replay_batch(
            cx,
            pool,
            config,
            cursor,
            &AtomicBool::new(false),
            &intents,
        ))
        .unwrap()
    }

    #[test]
    fn bulk_release_stays_pending_across_pages_cancellation_and_restart() {
        with_replay_mailbox(|cx, pool, config, _, _| {
            let cutoff = mcp_agent_mail_db::now_micros();
            for id in 1..=129 {
                seed_replay_lease(cx, pool, id, &format!("src/{id}.rs"), cutoff - 1);
            }
            queue_scope(config, cutoff, "BlueLake", None, None);
            let mut cursor = Cursor::default();
            let first = page_pass(cx, pool, config, &mut cursor);
            assert_eq!(
                (first.rows_released, first.completed, first.deferred),
                (64, 0, 0)
            );
            assert!(first.more);
            let original = fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &[1]))
                .into_result()
                .unwrap()[0]
                .released_ts;
            assert!(original.is_some());
            let log = journal::log_path(config, journal::RELEASE_INTENT_LOG_FILE);
            let before = std::fs::read(&log).unwrap();
            let queued = journal::read_queued_release_intents(config).unwrap();
            let stopped = fastmcp_core::block_on(replay_batch(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(true),
                &queued,
            ))
            .unwrap();
            assert!(stopped.interrupted);
            assert_eq!(stopped.attempted, 0);
            assert_eq!(std::fs::read(&log).unwrap(), before);
            // Restart loses only the cursor. Rescanning committed pages must
            // not change terminal timestamps or issue a premature receipt.
            let mut restarted = Cursor::default();
            for (released, completed) in [(0, 0), (64, 0), (1, 1)] {
                let report = page_pass(cx, pool, config, &mut restarted);
                assert_eq!(
                    (report.rows_released, report.completed, report.deferred),
                    (released, completed, 0)
                );
                assert_eq!(
                    journal::read_queued_release_intents(config)
                        .unwrap()
                        .is_empty(),
                    completed == 1
                );
            }
            let ids: Vec<_> = (1..=129).collect();
            let rows = fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &ids))
                .into_result()
                .unwrap();
            assert_eq!(rows.len(), 129);
            assert!(rows.iter().all(|row| row.released_ts.is_some()));
            assert_eq!(
                rows.iter()
                    .find(|row| row.id == Some(1))
                    .unwrap()
                    .released_ts,
                original
            );
        });
    }

    #[test]
    fn sparse_paths_advance_without_releasing_unmatched_or_future_leases() {
        for explicit_ids in [false, true] {
            with_replay_mailbox(|cx, pool, config, _, _| {
                let cutoff = mcp_agent_mail_db::now_micros();
                for id in 1..=128 {
                    seed_replay_lease(cx, pool, id, &format!("docs/{id}.md"), cutoff - 1);
                }
                seed_replay_lease(cx, pool, 129, "src/selected.rs", cutoff - 1);
                seed_replay_lease(cx, pool, 130, "src/future.rs", cutoff + 1);
                queue_scope(
                    config,
                    cutoff,
                    "BlueLake",
                    Some(vec!["src/*.rs".into()]),
                    explicit_ids.then(|| vec![129, 130]),
                );
                let mut cursor = Cursor::default();
                // A path-only request still walks bounded sparse pages. An
                // explicit-ID intersection must not walk unrelated history.
                let empty_pages = if explicit_ids { 0 } else { 2 };
                for _ in 0..empty_pages {
                    let report = page_pass(cx, pool, config, &mut cursor);
                    assert_eq!(
                        (
                            report.applied,
                            report.rows_released,
                            report.completed,
                            report.deferred
                        ),
                        (1, 0, 0, 0)
                    );
                    assert!(report.more);
                }
                let last = page_pass(cx, pool, config, &mut cursor);
                assert_eq!(
                    (last.rows_released, last.completed, last.deferred),
                    (1, 1, 0)
                );
                assert!(!last.more);
                assert_eq!(
                    journal::read_queued_release_intents(config).unwrap(),
                    [] as [QueuedReleaseIntentView; 0]
                );
                let ids: Vec<_> = (1..=130).collect();
                let rows = fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &ids))
                    .into_result()
                    .unwrap();
                assert_eq!(rows.len(), 130);
                for row in rows {
                    assert_eq!(row.released_ts.is_some(), row.id == Some(129));
                }
            });
        }
    }

    #[test]
    fn small_closeout_progresses_beside_a_bulk_intent_and_a_failed_intent() {
        with_replay_mailbox(|cx, pool, config, _, _| {
            let cutoff = mcp_agent_mail_db::now_micros();
            for id in 1..=65 {
                seed_replay_lease(cx, pool, id, &format!("src/{id}.rs"), cutoff - 1);
            }
            let conn = fastmcp_core::block_on(pool.acquire(cx))
                .into_result()
                .unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(82, 71, 'GreenStone', 'test', 'test', 1, 1)").unwrap();
            conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts) VALUES(501, 71, 82, 'small.rs', 1, '', 1, {})", cutoff + 1_000_000)).unwrap();
            drop(conn);
            queue_scope(config, cutoff, "BlueLake", None, None);
            queue_scope(config, cutoff, "GreenStone", None, Some(vec![501]));
            queue_scope(
                config,
                cutoff - 1,
                "BlueLake",
                Some(vec!["../escape".into()]),
                None,
            );
            let mut cursor = Cursor::default();
            let first = page_pass(cx, pool, config, &mut cursor);
            assert_eq!(
                (
                    first.attempted,
                    first.completed,
                    first.deferred,
                    first.rows_released
                ),
                (3, 1, 1, 65)
            );
            let queued = journal::read_queued_release_intents(config).unwrap();
            assert_eq!(queued.len(), 2);
            assert!(queued.iter().all(|intent| intent.agent_name == "BlueLake"));
            let second = page_pass(cx, pool, config, &mut cursor);
            assert_eq!(
                (second.completed, second.deferred, second.rows_released),
                (1, 1, 1)
            );
            let queued = journal::read_queued_release_intents(config).unwrap();
            assert_eq!(queued.len(), 1);
            assert_eq!(queued[0].paths, Some(vec!["../escape".to_string()]));
        });
    }

    #[test]
    fn failed_readonly_admission_preserves_a_partial_scan_and_its_journal() {
        with_replay_mailbox(|cx, pool, config, _, _| {
            let cutoff = mcp_agent_mail_db::now_micros();
            for id in 1..=65 {
                seed_replay_lease(cx, pool, id, &format!("src/{id}.rs"), cutoff - 1);
            }
            queue_scope(config, cutoff, "BlueLake", None, None);
            let mut cursor = Cursor::default();
            assert_eq!(page_pass(cx, pool, config, &mut cursor).rows_released, 64);
            let readonly = DbPool::new_query_only(&super::super::pool_config(config)).unwrap();
            let log = journal::log_path(config, journal::RELEASE_INTENT_LOG_FILE);
            let before = std::fs::read(&log).unwrap();
            let queued = journal::read_queued_release_intents(config).unwrap();
            assert!(
                fastmcp_core::block_on(replay_batch(
                    cx,
                    &readonly,
                    config,
                    &mut cursor,
                    &AtomicBool::new(false),
                    &queued,
                ))
                .is_err()
            );
            assert_eq!(std::fs::read(&log).unwrap(), before);
            let resumed = page_pass(cx, pool, config, &mut cursor);
            assert_eq!(
                (resumed.rows_released, resumed.completed, resumed.deferred),
                (1, 1, 0)
            );
            assert_eq!(
                journal::read_queued_release_intents(config).unwrap(),
                [] as [QueuedReleaseIntentView; 0]
            );
        });
    }

    // These boundary tests inspect real process-global admission. Run in a
    // private child; file-backed output cannot fill a pipe while we wait.
    fn isolated_completion_test() -> bool {
        use std::io::{Read as _, Seek as _, SeekFrom};
        use std::time::{Duration, Instant};

        const CHILD: &str = "AM_TEST_RELEASE_COMPLETION_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named libtest thread");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let mut log = tempfile::tempfile().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .stdout(log.try_clone().unwrap())
            .stderr(log.try_clone().unwrap())
            .spawn()
            .expect("spawn isolated release completion test");
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut expired = false;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                expired = true;
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = child.wait().unwrap();
        log.seek(SeekFrom::Start(0)).unwrap();
        let mut output = String::new();
        log.read_to_string(&mut output).unwrap();
        assert!(
            !expired && status.success() && output.contains("1 passed; 0 failed"),
            "isolated {name} failed (timeout={expired}): {output}"
        );
        true
    }

    #[test]
    fn all_batch_receipts_survive_an_exit_before_the_first_archive_followup() {
        use mcp_agent_mail_db::write_barrier::{
            active_writer_count, try_acquire_promotion_barrier_if_idle,
        };
        use std::sync::Arc;

        if isolated_completion_test() {
            return;
        }
        with_replay_mailbox(|cx, pool, config, generation, _| {
            let cutoff = mcp_agent_mail_db::now_micros();
            let ids: Vec<_> = (401..407).collect();
            for &id in &ids {
                seed_replay_lease(cx, pool, id, &format!("src/{id}.rs"), cutoff - 1);
                queue_fixture_release(config, cutoff, id);
            }
            let reached = Arc::new(AtomicBool::new(false));
            let observed = Arc::clone(&reached);
            let selected = config.clone();
            BEFORE_ARCHIVE_FOLLOWUPS.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move |retained| {
                    assert_eq!(retained, MAX_ARCHIVE_REPAIRS_PER_BATCH);
                    assert!(journal::read_queued_release_intents(&selected).unwrap().is_empty());
                    assert_eq!(active_writer_count(), 0);
                    let promotion = try_acquire_promotion_barrier_if_idle()
                        .expect("completed batch must release database admission");
                    drop(promotion);
                    assert!(!selected.storage_root.join("projects/replay/file_reservations").exists());
                    // Mark only after every boundary assertion succeeded, so
                    // catch_unwind cannot mistake an assertion for the fault.
                    observed.store(true, Ordering::Release);
                    panic!("injected exit after all receipts, before archive I/O");
                }));
            });
            let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                page_pass(cx, pool, config, &mut Cursor::default())
            }));
            assert!(failed.is_err());
            assert!(reached.load(Ordering::Acquire));
            assert!(journal::read_queued_release_intents(config).unwrap().is_empty());
            let released = fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &ids))
                .into_result().unwrap();
            assert_eq!(released.len(), ids.len());
            assert!(released.iter().all(|row| row.released_ts.is_some()));
            assert_eq!(active_writer_count(), 0);

            // No intent or in-memory follow-up survives. The real history
            // reconciler must use the ledger and the latest database metadata.
            let conn = fastmcp_core::block_on(pool.acquire(cx)).into_result().unwrap();
            conn.execute_raw("UPDATE file_reservations SET reason='after replay exit'").unwrap();
            drop(conn);
            let mut history = mcp_agent_mail_storage::recovery::reservation_reconcile::ReservationReconcileCursor::default();
            for _ in 0..3 {
                mcp_agent_mail_storage::recovery::reservation_reconcile::reconcile_reservation_releases(
                    cx, pool, config, &mut history, &AtomicBool::new(false),
                ).unwrap();
            }
            for row in released {
                let file = mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
                    Some(generation), row.id.unwrap(),
                );
                let bytes = std::fs::read(config.storage_root.join("projects/replay/file_reservations").join(file)).unwrap();
                let artifact: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(artifact["reason"], "after replay exit");
                assert_eq!(artifact["db_generation"], generation);
                assert_eq!(artifact["released_ts"], micros_to_iso(row.released_ts.unwrap()));
            }
        });
    }

    #[test]
    fn shutdown_after_receipts_preserves_partial_pages_and_completed_closeouts() {
        use std::sync::Arc;

        if isolated_completion_test() {
            return;
        }
        with_replay_mailbox(|cx, pool, config, _, _| {
            let cutoff = mcp_agent_mail_db::now_micros();
            for id in 1..=65 {
                seed_replay_lease(cx, pool, id, &format!("src/{id}.rs"), cutoff - 1);
            }
            seed_replay_lease(cx, pool, 501, "small.rs", cutoff - 1);
            queue_scope(config, cutoff, "BlueLake", None, None);
            queue_fixture_release(config, cutoff, 501);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_at_boundary = Arc::clone(&stop);
            let selected = config.clone();
            BEFORE_ARCHIVE_FOLLOWUPS.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move |retained| {
                    assert_eq!(retained, MAX_ARCHIVE_REPAIRS_PER_BATCH);
                    let pending = journal::read_queued_release_intents(&selected).unwrap();
                    assert_eq!(pending.len(), 1);
                    assert!(pending[0].file_reservation_ids.is_none(), "bulk scope is incomplete");
                    assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
                    stop_at_boundary.store(true, Ordering::Release);
                }));
            });
            let mut cursor = Cursor::default();
            let intents = journal::read_queued_release_intents(config).unwrap();
            let report = fastmcp_core::block_on(replay_batch(
                cx, pool, config, &mut cursor, &stop, &intents,
            )).unwrap();
            assert!(report.interrupted);
            assert_eq!((report.applied, report.completed, report.rows_released), (2, 1, 65));
            assert!(!config.storage_root.join("projects/replay/file_reservations").exists());
            let before = fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &[1, 501]))
                .into_result().unwrap();
            assert!(before.iter().all(|row| row.released_ts.is_some()));
            let resumed = page_pass(cx, pool, config, &mut cursor);
            assert_eq!((resumed.rows_released, resumed.completed, resumed.deferred), (1, 1, 0));
            assert!(journal::read_queued_release_intents(config).unwrap().is_empty());
            let after = fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &[1, 501]))
                .into_result().unwrap();
            assert_eq!(
                before.iter().map(|row| (row.id, row.released_ts)).collect::<Vec<_>>(),
                after.iter().map(|row| (row.id, row.released_ts)).collect::<Vec<_>>(),
            );
        });
    }

    #[test]
    fn failed_receipt_publication_keeps_applied_release_pending_for_idempotent_retry() {
        if isolated_completion_test() {
            return;
        }
        with_replay_mailbox(|cx, pool, config, _, _| {
            let cutoff = mcp_agent_mail_db::now_micros();
            seed_replay_lease(cx, pool, 401, "src/receipt.rs", cutoff - 1);
            queue_fixture_release(config, cutoff, 401);
            let log = journal::log_path(config, journal::RELEASE_INTENT_LOG_FILE);
            let before = std::fs::read(&log).unwrap();
            let lock_path = config.storage_root.join(journal::DEGRADED_INTENTS_DIR).join(RELEASE_LOCK);
            let held = std::fs::OpenOptions::new().read(true).write(true).open(lock_path).unwrap();
            fs2::FileExt::try_lock_exclusive(&held).unwrap();
            let mut cursor = Cursor::default();
            let report = page_pass(cx, pool, config, &mut cursor);
            assert_eq!((report.rows_released, report.completed, report.deferred), (1, 0, 1));
            assert_eq!(std::fs::read(&log).unwrap(), before);
            assert_eq!(journal::read_queued_release_intents(config).unwrap().len(), 1);
            let original = fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &[401]))
                .into_result().unwrap()[0].released_ts;
            assert!(original.is_some());
            fs2::FileExt::unlock(&held).unwrap();
            let retry = page_pass(cx, pool, config, &mut cursor);
            assert_eq!((retry.rows_released, retry.completed, retry.deferred), (0, 1, 0));
            assert!(journal::read_queued_release_intents(config).unwrap().is_empty());
            let after = fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &[401]))
                .into_result().unwrap()[0].released_ts;
            assert_eq!(after, original);
        });
    }

    #[test]
    fn a_changed_generation_after_completion_cannot_authorize_old_archive_followups() {
        use mcp_agent_mail_db::write_barrier::{
            active_writer_count, try_acquire_promotion_barrier_if_idle,
        };

        if isolated_completion_test() {
            return;
        }
        with_replay_mailbox(|cx, pool, config, generation, _| {
            assert_ne!(generation, "ccdd");
            let cutoff = mcp_agent_mail_db::now_micros();
            seed_replay_lease(cx, pool, 401, "src/generation.rs", cutoff - 1);
            queue_fixture_release(config, cutoff, 401);
            let selected = config.clone();
            let database = pool.sqlite_path().to_string();
            BEFORE_ARCHIVE_FOLLOWUPS.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move |retained| {
                    assert_eq!(retained, 1);
                    assert!(journal::read_queued_release_intents(&selected).unwrap().is_empty());
                    assert_eq!(active_writer_count(), 0);
                    let promotion = try_acquire_promotion_barrier_if_idle().unwrap();
                    let conn = mcp_agent_mail_db::DbConn::open_file(&database).unwrap();
                    conn.execute_raw("UPDATE db_identity SET generation_id='ccdd' WHERE singleton=0").unwrap();
                    drop(conn);
                    drop(promotion);
                }));
            });
            let report = page_pass(cx, pool, config, &mut Cursor::default());
            assert_eq!((report.rows_released, report.completed, report.deferred), (1, 1, 0));
            assert!(journal::read_queued_release_intents(config).unwrap().is_empty());
            assert!(!config.storage_root.join("projects/replay/file_reservations").exists());
            let current = fastmcp_core::block_on(queries::db_generation_id(cx, pool))
                .into_result().unwrap();
            assert_eq!(current.as_deref(), Some("ccdd"));
        });
    }

    #[test]
    fn promotion_refusal_preserves_an_applied_bulk_page_and_its_durable_intent() {
        use mcp_agent_mail_db::write_barrier::{
            DrainOutcome, acquire_promotion_barrier_draining, active_writer_count,
            begin_write_activity, try_acquire_promotion_barrier_if_idle,
        };
        use std::sync::mpsc;
        use std::time::Duration;

        with_replay_mailbox(|cx, pool, config, _, _| {
            let cutoff = mcp_agent_mail_db::now_micros();
            for id in 1..=65 {
                seed_replay_lease(cx, pool, id, &format!("src/{id}.rs"), cutoff - 1);
            }
            queue_scope(config, cutoff, "BlueLake", None, None);
            let mut cursor = Cursor::default();
            assert_eq!(page_pass(cx, pool, config, &mut cursor).rows_released, 64);
            let round_before = cursor.round.after.clone();
            let progress_before: Vec<_> = cursor.pages.iter()
                .map(|(key, position)| (key.clone(), position.released)).collect();
            assert_eq!(progress_before.len(), 1);
            assert_eq!(progress_before[0].1, 64);
            let log = journal::log_path(config, journal::RELEASE_INTENT_LOG_FILE);
            let bytes_before = std::fs::read(&log).unwrap();
            let intents = journal::read_queued_release_intents(config).unwrap();
            for parent_writer in [false, true] {
                let parent = parent_writer.then(begin_write_activity);
                let (ready_tx, ready_rx) = mpsc::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let owner = std::thread::spawn(move || {
                    let gate = if parent_writer {
                        let (gate, result) = acquire_promotion_barrier_draining(Duration::ZERO);
                        assert!(matches!(result, DrainOutcome::TimedOut { remaining_writers: 1 }));
                        gate
                    } else {
                        try_acquire_promotion_barrier_if_idle().expect("idle promotion")
                    };
                    ready_tx.send(()).unwrap();
                    // Cleanup only: the test requires replay to return before
                    // explicit release, not by waiting for this timeout.
                    let released = release_rx.recv_timeout(Duration::from_secs(10));
                    drop(gate);
                    released
                });
                ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let refused = fastmcp_core::block_on(replay_batch(
                    cx, pool, config, &mut cursor, &AtomicBool::new(false), &intents,
                ));
                let writers = active_writer_count();
                let bytes_during = std::fs::read(&log).unwrap();
                drop(parent);
                let _ = release_tx.send(());
                assert!(owner.join().unwrap().is_ok(), "replay waited for promotion expiry");
                assert!(refused.unwrap_err().contains("admission contention"));
                assert_eq!(writers, usize::from(parent_writer));
                assert_eq!(active_writer_count(), 0);
                assert_eq!(bytes_during, bytes_before);
                assert_eq!(cursor.round.after, round_before);
                let progress: Vec<_> = cursor.pages.iter()
                    .map(|(key, position)| (key.clone(), position.released)).collect();
                assert_eq!(progress, progress_before);
                let remaining = fastmcp_core::block_on(queries::get_reservations_by_ids(cx, pool, &[65]))
                    .into_result().unwrap();
                assert!(remaining[0].released_ts.is_none());
            }
            // No cursor reset: completion must resume after the committed
            // 64-ID page, not rescan it or abandon the last lease.
            let resumed = page_pass(cx, pool, config, &mut cursor);
            assert_eq!((resumed.rows_released, resumed.completed, resumed.deferred), (1, 1, 0));
            assert!(journal::read_queued_release_intents(config).unwrap().is_empty());
        });
    }

    #[test]
    fn release_replay_cannot_use_a_promotion_owners_blocking_writer_exemption() {
        use mcp_agent_mail_db::write_barrier::{
            active_writer_count, try_acquire_promotion_barrier_if_idle,
        };

        with_replay_mailbox(|cx, pool, config, _, _| {
            let cutoff = mcp_agent_mail_db::now_micros();
            seed_replay_lease(cx, pool, 401, "src/owner.rs", cutoff - 1);
            queue_fixture_release(config, cutoff, 401);
            let intents = journal::read_queued_release_intents(config).unwrap();
            let log = journal::log_path(config, journal::RELEASE_INTENT_LOG_FILE);
            let before = std::fs::read(&log).unwrap();
            let mut cursor = Cursor::default();
            let gate = try_acquire_promotion_barrier_if_idle().unwrap();
            let refused = fastmcp_core::block_on(replay_batch(
                cx, pool, config, &mut cursor, &AtomicBool::new(false), &intents,
            ));
            let writers = active_writer_count();
            drop(gate);
            assert!(refused.unwrap_err().contains("admission contention"));
            assert_eq!(writers, 0);
            assert!(cursor.round.after.is_none());
            assert!(cursor.pages.is_empty());
            assert_eq!(std::fs::read(log).unwrap(), before);
            let resumed = page_pass(cx, pool, config, &mut cursor);
            assert_eq!((resumed.rows_released, resumed.completed), (1, 1));
        });
    }

    #[test]
    fn other_counted_writers_do_not_disable_release_replay() {
        use mcp_agent_mail_db::write_barrier::{active_writer_count, begin_write_activity};

        with_replay_mailbox(|cx, pool, config, _, _| {
            let cutoff = mcp_agent_mail_db::now_micros();
            seed_replay_lease(cx, pool, 401, "src/parallel.rs", cutoff - 1);
            queue_fixture_release(config, cutoff, 401);
            let writer = begin_write_activity();
            let report = page_pass(cx, pool, config, &mut Cursor::default());
            assert_eq!((report.rows_released, report.completed, report.deferred), (1, 1, 0));
            assert!(journal::read_queued_release_intents(config).unwrap().is_empty());
            assert_eq!(active_writer_count(), 1);
            drop(writer);
            assert_eq!(active_writer_count(), 0);
        });
    }
}
