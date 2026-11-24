//! Keyset pages for durable reservation-release replay.
//!
//! A page bounds rows materialized, not SQLite's internal scan work. Callers
//! retain the database write-activity lease while resolving identities, reading
//! a page and applying it. Progress is disposable: after a restart the durable
//! intent is scanned again and already-terminal releases remain idempotent.

use std::collections::{BTreeMap, BTreeSet};

use asupersync::Cx;
use mcp_agent_mail_db::DbPool;

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
    /// Last fully examined group of ID selectors. Each group releases its
    /// exact-path siblings with a separate bounded row cursor in `after`.
    requested_after: i64,
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
        if let Some(after) = page.finished_requested {
            self.requested_after = after;
            self.after = 0;
        } else {
            self.after = page.after;
        }
        self.released = self.released.saturating_add(released);
    }
}

pub(super) struct Page {
    pub(super) ids: Vec<i64>,
    after: i64,
    finished_requested: Option<i64>,
    pub(super) complete: bool,
}

/// Select a finite page before loading reservation payloads. Released rows are
/// deliberately included: the typed query owns release-ledger interpretation,
/// and advancing by selected IDs also makes sparse path filters progress.
/// The frozen upper ID prevents new arrivals from extending an existing scan.
/// Explicit-ID intents materialize bounded pages of exact-path siblings
/// selected by a bounded group of requested IDs. Both selectors and targets
/// must belong to the original owner and cutoff.
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
    if position.ceiling.is_none() {
        position.ceiling = Some(super::super::db_value(
            mcp_agent_mail_db::queries::get_release_replay_ceiling(cx, pool).await,
        )?);
    }
    let ceiling = position.ceiling.unwrap_or(0);
    if let Some(requested) = requested_ids {
        return sibling_page(cx, pool, cutoff, position, requested).await;
    }
    let ids = super::super::db_value(
        mcp_agent_mail_db::queries::get_release_replay_candidate_ids(
            cx,
            pool,
            authority.project_id,
            authority.agent_id,
            cutoff,
            position.after,
            ceiling,
            None,
            i64::try_from(PAGE_SIZE).expect("bounded page size"),
        )
        .await,
    )?;
    decode_page(ids, position.after, ceiling)
}

/// Resolve at most one group of selector IDs and one page of their siblings.
/// A selecting row released by an earlier partial apply remains authority for
/// this intent. A row released before the intent, acquired afterward, belonging
/// to another identity, or beyond the frozen ceiling cannot select any path.
async fn sibling_page(
    cx: &Cx,
    pool: &DbPool,
    cutoff: i64,
    position: &Position,
    requested: &[i64],
) -> Result<Page, String> {
    let authority = position
        .authority
        .as_ref()
        .ok_or_else(|| "release page has no resolved authority".to_string())?;
    let ceiling = position.ceiling.unwrap_or(0);
    let selectors = requested_page(cx, requested, position.requested_after, ceiling)?;
    if selectors.ids.is_empty() {
        return Ok(selectors);
    }
    let selected = super::super::db_value(
        mcp_agent_mail_db::queries::get_release_replay_selectors(
            cx,
            pool,
            authority.project_id,
            authority.agent_id,
            &selectors.ids,
            cutoff,
        )
        .await,
    )?;
    if selected.len() > PAGE_SIZE {
        return Err("release selector payload exceeded its row limit".to_string());
    }
    let paths = selected
        .into_iter()
        .map(|row| row.path_pattern)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(Page {
            ids: Vec::new(),
            after: position.after,
            finished_requested: Some(selectors.after),
            complete: selectors.complete,
        });
    }
    cx.checkpoint()
        .map_err(|_| "release sibling selection cancelled".to_string())?;
    let ids = super::super::db_value(
        mcp_agent_mail_db::queries::get_release_replay_candidate_ids(
            cx,
            pool,
            authority.project_id,
            authority.agent_id,
            cutoff,
            position.after,
            ceiling,
            Some(&paths),
            i64::try_from(PAGE_SIZE).expect("bounded page size"),
        )
        .await,
    )?;
    let mut page = decode_page(ids, position.after, ceiling)?;
    page.finished_requested = page.complete.then_some(selectors.after);
    page.complete &= selectors.complete;
    Ok(page)
}

fn decode_page(ids: Vec<i64>, mut after: i64, ceiling: i64) -> Result<Page, String> {
    if ids.len() > PAGE_SIZE {
        return Err("release page exceeded its row limit".to_string());
    }
    for &id in &ids {
        if id <= after || id <= 0 || id > ceiling {
            return Err("release page IDs are outside the ordered scan window".to_string());
        }
        after = id;
    }
    Ok(Page {
        complete: ids.len() < PAGE_SIZE,
        ids,
        after,
        finished_requested: None,
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
        finished_requested: None,
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
                finished_requested: None,
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
                    finished_requested: None,
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
    fn explicit_id_replay_rejects_newer_foreign_and_terminal_selectors() {
        with_id_replay_mailbox(|cx, pool, config| {
            let conn = block_on(pool.acquire(cx)).into_result().unwrap();
            let future = mcp_agent_mail_db::now_micros() + 3_600_000_000;
            conn.execute_raw(&format!(
                "INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \
                 \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES \
                 (1, 71, 81, 'src/a.rs', 1, '', 1, {future}, NULL), \
                 (2, 71, 81, 'src/a.rs', 1, '', 11, {future}, NULL), \
                 (3, 71, 81, 'src/b.rs', 1, '', 1, {future}, NULL), \
                 (4, 71, 81, 'src/b.rs', 1, '', 1, {future}, 2), \
                 (5, 71, 81, 'src/c.rs', 1, '', 1, {future}, NULL), \
                 (6, 71, 81, 'src/c.rs', 1, '', 1, {future}, NULL), \
                 (7, 71, 82, 'src/d.rs', 1, '', 1, {future}, NULL), \
                 (8, 71, 81, 'src/d.rs', 1, '', 1, {future}, NULL), \
                 (9, 72, 83, 'src/e.rs', 1, '', 1, {future}, NULL), \
                 (10, 71, 81, 'src/e.rs', 1, '', 1, {future}, NULL), \
                 (11, 71, 81, 'src/f.rs', 1, '', 1, {future}, NULL), \
                 (12, 71, 81, 'src/f.rs', 1, '', 1, {future}, NULL), \
                 (13, 71, 81, 'src/g.rs', 1, '', 1, {future}, NULL), \
                 (14, 71, 81, 'src/g.rs', 1, '', 1, {future}, NULL), \
                 (15, 71, 81, 'src/h.rs', 1, '', 1, {future}, NULL), \
                 (16, 71, 81, 'src/h.rs', 1, '', 1, {future}, 1), \
                 (17, 71, 81, 'src/i.rs', 1, '', 1, {future}, NULL), \
                 (18, 71, 81, 'src/i.rs', 1, '', 1, {future}, 20), \
                 (19, 71, 81, 'src/j.rs', 1, '', 1, {future}, NULL), \
                 (20, 71, 81, 'src/j.rs', 1, '', 1, {future}, 'garbage'), \
                 (21, 71, 81, 'src/k.rs', 1, '', 1, {future}, NULL), \
                 (22, 71, 81, 'src/k.rs', 1, '', 1, {future}, NULL), \
                 (23, 71, 81, 'src/l.rs', 1, '', 1, {future}, NULL), \
                 (24, 71, 81, 'src/l.rs', 1, '', 1, {future}, NULL), \
                 (25, 71, 81, 'src/m.rs', 1, '', 1, {future}, NULL), \
                 (26, 71, 81, 'src/m.rs', 1, '', 1, {future}, 20), \
                 (27, 71, 81, 'src/n.rs', 1, '', 1, {future}, NULL), \
                 (28, 71, 81, 'src/n.rs', 1, '', 1, {future}, NULL), \
                 (29, 71, 81, 'src/o.rs', 1, '', 1, {future}, NULL), \
                 (30, 71, 81, 'src/o.rs', 1, '', 1, {future}, 'none')"
            ))
            .unwrap();
            conn.execute_raw(
                "INSERT INTO file_reservation_releases(reservation_id, released_ts) VALUES \
                 (6, 2), (14, 0), (16, 20), (18, 1), (22, 'garbage'), (24, -1), (28, 20)",
            )
            .unwrap();
            drop(conn);
            queue_requested_ids(
                config,
                &[2, 4, 6, 7, 9, 11, 14, 16, 18, 20, 22, 24, 26, 28, 30],
                serde_json::Value::Null,
            );
            let report = replay_requested_ids(cx, pool, config, &mut Cursor::default());
            assert_eq!(
                (report.rows_released, report.completed, report.deferred),
                (6, 1, 0)
            );
            assert!(!report.more);
            // The general typed query rejects malformed terminal payloads;
            // inspect those directly below instead of hiding decode errors.
            let ids: Vec<_> = (1..=30).filter(|id| ![20, 22].contains(id)).collect();
            let rows = block_on(queries::get_reservations_by_ids(cx, pool, &ids))
                .into_result()
                .unwrap();
            assert_eq!(rows.len(), 28);
            for row in rows {
                match row.id.unwrap() {
                    4 | 6 => assert_eq!(row.released_ts, Some(2)),
                    14 => assert_eq!(row.released_ts, Some(0)),
                    16 | 26 | 28 => assert_eq!(row.released_ts, Some(20)),
                    18 => assert_eq!(row.released_ts, Some(1)),
                    24 => assert_eq!(row.released_ts, Some(-1)),
                    11 | 12 | 25 | 27 | 29 | 30 => {
                        assert!(row.released_ts.is_some_and(|ts| ts >= 10));
                    }
                    _ => assert!(row.released_ts.is_none(), "out-of-scope selector: {row:?}"),
                }
            }
            let conn = block_on(pool.acquire(cx)).into_result().unwrap();
            assert_eq!(
                conn.query_sync(
                    "SELECT released_ts FROM file_reservations WHERE id = 20",
                    &[]
                )
                .unwrap()[0]
                    .get_named::<String>("released_ts")
                    .unwrap(),
                "garbage"
            );
            assert_eq!(
                conn.query_sync(
                    "SELECT released_ts FROM file_reservation_releases WHERE reservation_id = 22",
                    &[]
                )
                .unwrap()[0]
                    .get_named::<String>("released_ts")
                    .unwrap(),
                "garbage"
            );
            drop(conn);
            assert!(
                journal::read_queued_release_intents(config)
                    .unwrap()
                    .is_empty()
            );
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
            conn.execute_raw("UPDATE file_reservations SET released_ts = 2 WHERE id = 4")
                .unwrap();
            drop(conn);
            let mut requested: Vec<_> = (1..=65).rev().collect();
            requested.extend([0, -1, 1, 500]);
            queue_requested_ids(config, &requested, serde_json::Value::Null);
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
                match row.id.unwrap() {
                    4 => assert_eq!(row.released_ts, Some(2)),
                    65 => assert!(row.released_ts.is_some_and(|ts| ts >= 10)),
                    _ => assert!(row.released_ts.is_none()),
                }
            }
        });
    }
}
