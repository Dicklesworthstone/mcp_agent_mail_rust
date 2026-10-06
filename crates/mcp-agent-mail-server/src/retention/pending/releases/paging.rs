//! Keyset pages for durable reservation-release replay.
//!
//! A page bounds rows materialized, not SQLite's internal scan work. Callers
//! retain the database write-activity lease while resolving identities, reading
//! a page and applying it. Progress is disposable: after a restart the durable
//! intent is scanned again and already-terminal releases remain idempotent.

use std::collections::{BTreeMap, BTreeSet};

use asupersync::{Cx, Outcome};
use mcp_agent_mail_db::{DbError, DbPool};

use super::super::{IntentKey, RoundCursor};

pub(super) const PAGE_SIZE: usize = 64;
const MAX_TRACKED_INTENTS: usize = 65_536;

/// The journal scanner admits no more identities than this cursor can retain.
/// Do not evict an unfinished page merely to admit a newer intent.
#[derive(Default)]
pub(in crate::retention::pending) struct Cursor {
    pub(super) round: RoundCursor,
    pub(super) pages: BTreeMap<IntentKey, Position>,
    source_identity: String,
}

impl Cursor {
    pub(super) fn prepare(&mut self, identity: String, keys: &[IntentKey]) -> Result<(), String> {
        if keys.len() > MAX_TRACKED_INTENTS {
            return Err("release replay cursor identity bound exceeded".to_string());
        }
        if self.source_identity != identity {
            self.round = RoundCursor::default();
            self.pages.clear();
            self.source_identity = identity;
        }
        let pending: BTreeSet<_> = keys.iter().collect();
        self.pages.retain(|key, _| pending.contains(key));
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Authority {
    project_id: i64,
    agent_id: i64,
    generation: Option<String>,
}

#[derive(Default)]
pub(super) struct Position {
    authority: Option<Authority>,
    after: i64,
    ceiling: Option<i64>,
    /// Confirmed rows released during this cursor's lifetime, not a durable
    /// lifetime counter. A restart may re-observe earlier successful releases.
    pub(super) released: usize,
}

impl Position {
    pub(super) fn bind(&mut self, project_id: i64, agent_id: i64, generation: Option<String>) {
        let authority = Authority {
            project_id,
            agent_id,
            generation,
        };
        if self.authority.as_ref() != Some(&authority) {
            *self = Self {
                authority: Some(authority),
                ..Self::default()
            };
        }
    }

    /// Call only after a successful database outcome, even for a page with no
    /// matching paths. Failed/ambiguous mutations must retry the same page.
    pub(super) fn applied(&mut self, page: &Page, released: usize) {
        self.after = page.after;
        self.released = self.released.saturating_add(released);
    }
}

pub(super) struct Page {
    pub(super) ids: Vec<i64>,
    after: i64,
    pub(super) complete: bool,
}

fn source_error(error: impl std::fmt::Display) -> String {
    let error = DbError::Sqlite(error.to_string());
    mcp_agent_mail_db::corruption_circuit_breaker().observe_error(&error);
    error.to_string()
}

/// Select a finite page before loading reservation payloads. Released rows are
/// deliberately included: the typed query owns release-ledger interpretation,
/// and advancing by selected IDs also makes sparse path filters progress.
/// The frozen upper ID prevents new arrivals from extending an existing scan.
/// Explicit-ID intents page over their requested IDs instead of the owner's
/// entire reservation history. Those IDs are candidates, not authorization:
/// the caller must still enforce project, owner, path and creation cutoff.
pub(super) async fn select(
    cx: &Cx,
    pool: &DbPool,
    cutoff: i64,
    position: &mut Position,
    requested_ids: Option<&[i64]>,
) -> Result<Page, String> {
    cx.checkpoint()
        .map_err(|_| "release page selection cancelled".to_string())?;
    let authority = position
        .authority
        .as_ref()
        .ok_or_else(|| "release page has no resolved authority".to_string())?;
    if cutoff <= 0 || authority.project_id <= 0 || authority.agent_id <= 0 {
        return Err("release page requires positive identities and cutoff".to_string());
    }
    let conn = match pool.acquire(cx).await {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => return Err(source_error(error)),
        Outcome::Cancelled(_) => return Err("release page selection cancelled".to_string()),
        Outcome::Panicked(_) => return Err("release page selection panicked".to_string()),
    };
    if position.ceiling.is_none() {
        let rows = conn
            .query_sync(
                "SELECT COALESCE(MAX(id), 0) AS ceiling FROM file_reservations",
                &[],
            )
            .map_err(source_error)?;
        let ceiling = rows
            .first()
            .ok_or_else(|| "release page ceiling is missing".to_string())?
            .get_named::<i64>("ceiling")
            .map_err(source_error)?;
        position.ceiling = Some(ceiling.max(0));
    }
    let ceiling = position.ceiling.unwrap_or(0);
    if let Some(requested) = requested_ids {
        // Payload lookup and mutation already use page-bounded primary keys.
        // Do not walk unrelated historical leases just to find those keys.
        drop(conn);
        return requested_page(cx, requested, position.after, ceiling);
    }
    let rows = conn
        .query_sync(
            "SELECT id FROM file_reservations WHERE project_id = ? AND agent_id = ? \
         AND created_ts <= ? AND id > ? AND id <= ? ORDER BY id LIMIT ?",
            &[
                authority.project_id.into(),
                authority.agent_id.into(),
                cutoff.into(),
                position.after.into(),
                ceiling.into(),
                i64::try_from(PAGE_SIZE).expect("bounded page size").into(),
            ],
        )
        .map_err(source_error)?;
    let mut ids = Vec::with_capacity(rows.len().min(PAGE_SIZE));
    let mut after = position.after;
    if rows.len() > PAGE_SIZE {
        return Err("release page exceeded its row limit".to_string());
    }
    for row in rows {
        let id = row.get_named::<i64>("id").map_err(source_error)?;
        if id <= after || id <= 0 || id > ceiling {
            return Err("release page IDs are outside the ordered scan window".to_string());
        }
        after = id;
        ids.push(id);
    }
    Ok(Page {
        complete: ids.len() < PAGE_SIZE,
        ids,
        after,
    })
}

/// Keep only the next page and one lookahead ID, not a sorted copy of a
/// potentially large request. Advance by requested IDs even when their rows
/// are absent or fail authorization, otherwise sparse requests can stall or
/// appear complete before later requested IDs have been examined.
fn requested_page(cx: &Cx, requested: &[i64], after: i64, ceiling: i64) -> Result<Page, String> {
    let mut selected = BTreeSet::new();
    for chunk in requested.chunks(1024) {
        cx.checkpoint()
            .map_err(|_| "release ID page selection cancelled".to_string())?;
        for &id in chunk {
            if id <= 0 || id <= after || id > ceiling || selected.contains(&id) {
                continue;
            }
            if selected.len() == PAGE_SIZE + 1 {
                if selected.last().is_some_and(|last| id >= *last) {
                    continue;
                }
                // Remove before insertion to retain at most 65 IDs, including
                // lookahead. Duplicates must not evict an existing candidate.
                let _ = selected.pop_last();
            }
            selected.insert(id);
        }
    }
    let complete = selected.len() <= PAGE_SIZE;
    if !complete {
        let _ = selected.pop_last();
    }
    Ok(Page {
        after: selected.last().copied().unwrap_or(after),
        ids: selected.into_iter().collect(),
        complete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastmcp_core::block_on;
    use mcp_agent_mail_core::Config;
    use mcp_agent_mail_db::queries;
    use mcp_agent_mail_tools::degraded_intents as journal;
    use std::sync::atomic::AtomicBool;

    fn key(id: i64) -> IntentKey {
        (id, format!("{id:064x}"))
    }

    #[test]
    fn cursor_discards_completed_intents_and_resets_for_a_replaced_mailbox() {
        let mut cursor = Cursor::default();
        cursor.prepare("first".into(), &[key(1), key(2)]).unwrap();
        cursor.pages.insert(
            key(1),
            Position {
                after: 64,
                ..Position::default()
            },
        );
        cursor.pages.insert(key(2), Position::default());
        cursor.round.after = Some(key(1));
        cursor.prepare("first".into(), &[key(1)]).unwrap();
        assert_eq!(cursor.pages.len(), 1);
        assert_eq!(cursor.pages[&key(1)].after, 64);
        cursor.prepare("replacement".into(), &[key(1)]).unwrap();
        assert!(cursor.pages.is_empty());
        assert!(cursor.round.after.is_none());
        assert!(cursor.round.ceiling.is_none());
    }

    #[test]
    fn oversized_snapshot_does_not_evict_existing_progress() {
        let mut cursor = Cursor::default();
        cursor.prepare("mailbox".into(), &[key(1)]).unwrap();
        cursor.pages.insert(
            key(1),
            Position {
                after: 64,
                ..Position::default()
            },
        );
        assert!(
            cursor
                .prepare("other".into(), &vec![key(2); MAX_TRACKED_INTENTS + 1])
                .is_err()
        );
        assert_eq!(cursor.pages[&key(1)].after, 64);
        assert_eq!(cursor.source_identity, "mailbox");
    }

    #[test]
    fn generation_and_identity_changes_restart_the_scan_without_reusing_counts() {
        let mut position = Position::default();
        position.bind(71, 81, Some("aabb".into()));
        position.ceiling = Some(300);
        position.applied(
            &Page {
                ids: vec![64],
                after: 64,
                complete: false,
            },
            5,
        );
        position.bind(71, 81, Some("aabb".into()));
        assert_eq!(
            (position.after, position.ceiling, position.released),
            (64, Some(300), 5)
        );
        for (project, agent, generation) in [
            (71, 81, Some("ccdd".into())),
            (72, 81, Some("ccdd".into())),
            (72, 82, Some("ccdd".into())),
            (72, 82, None),
        ] {
            position.bind(project, agent, generation);
            assert_eq!(
                (position.after, position.ceiling, position.released),
                (0, None, 0)
            );
            position.applied(
                &Page {
                    ids: vec![90],
                    after: 90,
                    complete: false,
                },
                2,
            );
        }
    }

    #[test]
    fn real_database_pages_are_bounded_scoped_and_have_a_frozen_ceiling() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let pool = DbPool::new(&mcp_agent_mail_db::DbPoolConfig {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                    &temp.path().join("mail.sqlite3"),
                ),
                storage_root: Some(temp.path().join("archive")),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let cx = Cx::for_testing();
            let conn = fastmcp_core::block_on(pool.acquire(&cx))
                .into_result()
                .unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'pages', '/pages', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1), (82, 71, 'GreenStone', 'test', 'test', 1, 1)").unwrap();
            for id in 1..=129 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts) VALUES({id}, 71, 81, 'src/{id}.rs', 1, '', 1, 1000000)")).unwrap();
            }
            conn.execute_raw("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts) VALUES(200, 71, 82, 'foreign.rs', 1, '', 1, 1000000), (201, 71, 81, 'future.rs', 1, '', 11, 1000000)").unwrap();
            drop(conn);
            let mut position = Position::default();
            position.bind(71, 81, None);
            let first =
                fastmcp_core::block_on(select(&cx, &pool, 10, &mut position, None)).unwrap();
            assert_eq!(first.ids, (1..=64).collect::<Vec<_>>());
            assert!(!first.complete);
            assert_eq!(
                position.after, 0,
                "selection alone must not acknowledge a page"
            );
            let repeated =
                fastmcp_core::block_on(select(&cx, &pool, 10, &mut position, None)).unwrap();
            assert_eq!(repeated.ids, first.ids);
            position.applied(&first, 0);
            let conn = fastmcp_core::block_on(pool.acquire(&cx))
                .into_result()
                .unwrap();
            conn.execute_raw("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts) VALUES(500, 71, 81, 'late.rs', 1, '', 1, 1000000)").unwrap();
            drop(conn);
            let second =
                fastmcp_core::block_on(select(&cx, &pool, 10, &mut position, None)).unwrap();
            assert_eq!(second.ids, (65..=128).collect::<Vec<_>>());
            position.applied(&second, 0);
            let last = fastmcp_core::block_on(select(&cx, &pool, 10, &mut position, None)).unwrap();
            assert_eq!(last.ids, vec![129]);
            assert!(last.complete);
            assert_eq!(position.ceiling, Some(201));
        });
    }

    #[test]
    fn requested_id_pages_are_sorted_unique_and_bounded_without_skipping_duplicates() {
        let cx = Cx::for_testing();
        let mut requested: Vec<_> = (1..=129).rev().collect();
        requested.extend((1..=129).rev());
        requested.extend([0, -1, i64::MIN, i64::MAX, 500]);
        let mut after = 0;
        for (expected, complete) in [
            ((1..=64).collect::<Vec<_>>(), false),
            ((65..=128).collect(), false),
            (vec![129], true),
        ] {
            let page = requested_page(&cx, &requested, after, 129).unwrap();
            assert_eq!(page.ids, expected);
            assert_eq!(page.complete, complete);
            assert!(page.ids.len() <= PAGE_SIZE);
            let retry = requested_page(&cx, &requested, after, 129).unwrap();
            assert_eq!(retry.ids, page.ids, "selection alone cannot consume IDs");
            after = page.after;
        }
        let finished = requested_page(&cx, &requested, after, 129).unwrap();
        assert!(finished.ids.is_empty() && finished.complete);
        assert_eq!(finished.after, 129);
    }

    #[test]
    fn requested_id_page_completion_uses_unique_lookahead_not_raw_request_length() {
        let cx = Cx::for_testing();
        for count in [0, 1, 63, 64, 65] {
            let requested: Vec<i64> = (1..=count)
                .rev()
                .cycle()
                .take(count * 4)
                .map(|id| i64::try_from(id).unwrap())
                .collect();
            let page = requested_page(&cx, &requested, 0, 100).unwrap();
            assert_eq!(page.ids.len(), count.min(PAGE_SIZE));
            assert_eq!(page.complete, count <= PAGE_SIZE);
        }
        let empty = requested_page(&cx, &[0, -1, 64, 500], 64, 129).unwrap();
        assert!(empty.ids.is_empty() && empty.complete);
        assert_eq!(empty.after, 64);
    }

    #[test]
    fn requested_id_pages_match_a_full_sort_oracle_across_request_orderings() {
        let cx = Cx::for_testing();
        for multiplier in [1, 3, 5, 17, 129, 255] {
            let requested: Vec<i64> = (0..512)
                .map(|index| (index * multiplier) % 257 - 20)
                .collect();
            for after in [0, 1, 63, 64, 128, 200] {
                let expected: BTreeSet<_> = requested
                    .iter()
                    .copied()
                    .filter(|id| *id > 0 && *id > after && *id <= 220)
                    .collect();
                let page = requested_page(&cx, &requested, after, 220).unwrap();
                assert_eq!(page.complete, expected.len() <= PAGE_SIZE);
                assert_eq!(
                    page.ids,
                    expected.into_iter().take(PAGE_SIZE).collect::<Vec<_>>()
                );
            }
        }
    }

    fn with_id_replay_mailbox(test: impl FnOnce(&Cx, &DbPool, &Config)) {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let config = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                    &temp.path().join("mail.sqlite3"),
                ),
                storage_root: temp.path().join("archive"),
                ..Default::default()
            };
            std::fs::create_dir_all(&config.storage_root).unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir_all(&project).unwrap();
            let pool = DbPool::new(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let cx = Cx::for_testing();
            let conn = block_on(pool.acquire(&cx)).into_result().unwrap();
            let project_key = project.to_string_lossy().replace('\'', "''");
            conn.execute_raw(&format!(
                "INSERT INTO projects(id, slug, human_key, created_at) \
                 VALUES(71, 'id-pages', '{project_key}', 1), (72, 'foreign', '/foreign', 1)"
            ))
            .unwrap();
            conn.execute_raw(
                "INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) \
                 VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1), \
                 (82, 71, 'GreenStone', 'test', 'test', 1, 1), \
                 (83, 72, 'BlueLake', 'test', 'test', 1, 1)"
            ).unwrap();
            drop(conn);
            test(&cx, &pool, &config);
            mcp_agent_mail_storage::flush_async_commits();
        });
    }

    fn queue_requested_ids(config: &Config, ids: &[i64], paths: serde_json::Value) {
        let mut record = serde_json::json!({
            "schema_version": 1, "kind": journal::RELEASE_INTENT_KIND,
            "created_ts": 10, "project_key": "id-pages", "agent_name": "BlueLake",
            "paths": paths, "file_reservation_ids": ids,
            "failure": {"stage": "test", "error_detail": "database unavailable"},
        });
        let hash = journal::hash_json_value(&record);
        record["intent_id"] = serde_json::json!(&hash[..16]);
        record["content_sha256"] = serde_json::json!(hash);
        journal::append_jsonl(
            config,
            journal::RELEASE_INTENT_LOG_FILE,
            super::super::RELEASE_LOCK,
            &record,
        )
        .unwrap();
    }

    fn replay_requested_ids(
        cx: &Cx,
        pool: &DbPool,
        config: &Config,
        cursor: &mut Cursor,
    ) -> super::super::super::ReplayReport {
        let intents = journal::read_queued_release_intents(config).unwrap();
        block_on(super::super::replay_batch(
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
    fn explicit_release_reaches_its_lease_without_scanning_older_history() {
        with_id_replay_mailbox(|cx, pool, config| {
            let conn = block_on(pool.acquire(cx)).into_result().unwrap();
            for id in 1..=129 {
                conn.execute_raw(&format!(
                    "INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \
                     \"exclusive\", reason, created_ts, expires_ts, released_ts) \
                     VALUES({id}, 71, 81, 'history/{id}.rs', 1, '', 1, 9000000, 2)"
                ))
                .unwrap();
            }
            let future = mcp_agent_mail_db::now_micros() + 3_600_000_000;
            conn.execute_raw(&format!(
                "INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \
                 \"exclusive\", reason, created_ts, expires_ts) \
                 VALUES(900, 71, 81, 'src/target.rs', 1, '', 1, {future})"
            ))
            .unwrap();
            drop(conn);
            queue_requested_ids(config, &[900, 900], serde_json::Value::Null);
            let report = replay_requested_ids(cx, pool, config, &mut Cursor::default());
            assert_eq!(
                (report.attempted, report.completed, report.rows_released),
                (1, 1, 1)
            );
            assert_eq!(report.deferred, 0);
            assert!(!report.more);
            assert_eq!(
                journal::read_queued_release_intents(config).unwrap(),
                [] as [journal::QueuedReleaseIntentView; 0]
            );
            let rows = block_on(queries::get_reservations_by_ids(
                cx,
                pool,
                &[1, 64, 129, 900],
            ))
            .into_result()
            .unwrap();
            assert_eq!(rows.len(), 4);
            for row in rows {
                if row.id == Some(900) {
                    assert!(row.released_ts.is_some_and(|ts| ts > 2));
                } else {
                    assert_eq!(row.released_ts, Some(2), "history was not replayed");
                }
            }
        });
    }

    #[test]
    fn sparse_requested_pages_keep_scope_and_finish_only_after_the_last_candidate() {
        with_id_replay_mailbox(|cx, pool, config| {
            let conn = block_on(pool.acquire(cx)).into_result().unwrap();
            let future = mcp_agent_mail_db::now_micros() + 3_600_000_000;
            conn.execute_raw(&format!(
                "INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \
                 \"exclusive\", reason, created_ts, expires_ts) VALUES \
                 (1, 71, 82, 'src/owner.rs', 1, '', 1, {future}), \
                 (2, 72, 83, 'src/project.rs', 1, '', 1, {future}), \
                 (3, 71, 81, 'src/future.rs', 1, '', 11, {future}), \
                 (4, 71, 81, 'docs/keep.md', 1, '', 1, {future}), \
                 (65, 71, 81, 'src/target.rs', 1, '', 1, {future})"
            ))
            .unwrap();
            drop(conn);
            let mut requested: Vec<_> = (1..=65).rev().collect();
            requested.extend([0, -1, 1, 500]);
            queue_requested_ids(config, &requested, serde_json::json!(["src/*.rs"]));
            let mut cursor = Cursor::default();
            let first = replay_requested_ids(cx, pool, config, &mut cursor);
            assert_eq!(
                (
                    first.applied,
                    first.completed,
                    first.rows_released,
                    first.deferred
                ),
                (1, 0, 0, 0)
            );
            assert!(
                first.more,
                "an empty matching page is not an empty remaining request"
            );
            assert_eq!(
                journal::read_queued_release_intents(config).unwrap().len(),
                1
            );
            let conn = block_on(pool.acquire(cx)).into_result().unwrap();
            // Even a backdated new arrival cannot extend the frozen ID window.
            conn.execute_raw(&format!(
                "INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \
                 \"exclusive\", reason, created_ts, expires_ts) \
                 VALUES(500, 71, 81, 'src/late.rs', 1, '', 1, {future})"
            ))
            .unwrap();
            drop(conn);
            let second = replay_requested_ids(cx, pool, config, &mut cursor);
            assert_eq!(
                (second.completed, second.rows_released, second.deferred),
                (1, 1, 0)
            );
            assert!(!second.more);
            assert_eq!(
                journal::read_queued_release_intents(config).unwrap(),
                [] as [journal::QueuedReleaseIntentView; 0]
            );
            let rows = block_on(queries::get_reservations_by_ids(
                cx,
                pool,
                &[1, 2, 3, 4, 65, 500],
            ))
            .into_result()
            .unwrap();
            assert_eq!(rows.len(), 6);
            for row in rows {
                assert_eq!(row.released_ts.is_some_and(|ts| ts > 0), row.id == Some(65));
            }
        });
    }
}
