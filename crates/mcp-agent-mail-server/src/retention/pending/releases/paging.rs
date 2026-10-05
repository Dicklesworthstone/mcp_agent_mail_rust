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
        let authority = Authority { project_id, agent_id, generation };
        if self.authority.as_ref() != Some(&authority) {
            *self = Self { authority: Some(authority), ..Self::default() };
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
pub(super) async fn select(
    cx: &Cx,
    pool: &DbPool,
    cutoff: i64,
    position: &mut Position,
) -> Result<Page, String> {
    cx.checkpoint().map_err(|_| "release page selection cancelled".to_string())?;
    let authority = position.authority.as_ref()
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
        let rows = conn.query_sync(
            "SELECT COALESCE(MAX(id), 0) AS ceiling FROM file_reservations", &[],
        ).map_err(source_error)?;
        let ceiling = rows.first()
            .ok_or_else(|| "release page ceiling is missing".to_string())?
            .get_named::<i64>("ceiling").map_err(source_error)?;
        position.ceiling = Some(ceiling.max(0));
    }
    let ceiling = position.ceiling.unwrap_or(0);
    let rows = conn.query_sync(
        "SELECT id FROM file_reservations WHERE project_id = ? AND agent_id = ? \
         AND created_ts <= ? AND id > ? AND id <= ? ORDER BY id LIMIT ?",
        &[
            authority.project_id.into(), authority.agent_id.into(), cutoff.into(),
            position.after.into(), ceiling.into(),
            i64::try_from(PAGE_SIZE).expect("bounded page size").into(),
        ],
    ).map_err(source_error)?;
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
    Ok(Page { complete: ids.len() < PAGE_SIZE, ids, after })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: i64) -> IntentKey {
        (id, format!("{id:064x}"))
    }

    #[test]
    fn cursor_discards_completed_intents_and_resets_for_a_replaced_mailbox() {
        let mut cursor = Cursor::default();
        cursor.prepare("first".into(), &[key(1), key(2)]).unwrap();
        cursor.pages.insert(key(1), Position { after: 64, ..Position::default() });
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
        cursor.pages.insert(key(1), Position { after: 64, ..Position::default() });
        assert!(cursor.prepare("other".into(), &vec![key(2); MAX_TRACKED_INTENTS + 1]).is_err());
        assert_eq!(cursor.pages[&key(1)].after, 64);
        assert_eq!(cursor.source_identity, "mailbox");
    }

    #[test]
    fn generation_and_identity_changes_restart_the_scan_without_reusing_counts() {
        let mut position = Position::default();
        position.bind(71, 81, Some("aabb".into()));
        position.ceiling = Some(300);
        position.applied(&Page { ids: vec![64], after: 64, complete: false }, 5);
        position.bind(71, 81, Some("aabb".into()));
        assert_eq!((position.after, position.ceiling, position.released), (64, Some(300), 5));
        for (project, agent, generation) in [
            (71, 81, Some("ccdd".into())), (72, 81, Some("ccdd".into())),
            (72, 82, Some("ccdd".into())), (72, 82, None),
        ] {
            position.bind(project, agent, generation);
            assert_eq!((position.after, position.ceiling, position.released), (0, None, 0));
            position.applied(&Page { ids: vec![90], after: 90, complete: false }, 2);
        }
    }

    #[test]
    fn real_database_pages_are_bounded_scoped_and_have_a_frozen_ceiling() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let pool = DbPool::new(&mcp_agent_mail_db::DbPoolConfig {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&temp.path().join("mail.sqlite3")),
                storage_root: Some(temp.path().join("archive")),
                min_connections: 1, max_connections: 1,
                ..Default::default()
            }).unwrap();
            let cx = Cx::for_testing();
            let conn = fastmcp_core::block_on(pool.acquire(&cx)).into_result().unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'pages', '/pages', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1), (82, 71, 'GreenStone', 'test', 'test', 1, 1)").unwrap();
            for id in 1..=129 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts) VALUES({id}, 71, 81, 'src/{id}.rs', 1, '', 1, 1000000)")).unwrap();
            }
            conn.execute_raw("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts) VALUES(200, 71, 82, 'foreign.rs', 1, '', 1, 1000000), (201, 71, 81, 'future.rs', 1, '', 11, 1000000)").unwrap();
            drop(conn);
            let mut position = Position::default();
            position.bind(71, 81, None);
            let first = fastmcp_core::block_on(select(&cx, &pool, 10, &mut position)).unwrap();
            assert_eq!(first.ids, (1..=64).collect::<Vec<_>>());
            assert!(!first.complete);
            assert_eq!(position.after, 0, "selection alone must not acknowledge a page");
            let repeated = fastmcp_core::block_on(select(&cx, &pool, 10, &mut position)).unwrap();
            assert_eq!(repeated.ids, first.ids);
            position.applied(&first, 0);
            let conn = fastmcp_core::block_on(pool.acquire(&cx)).into_result().unwrap();
            conn.execute_raw("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts) VALUES(500, 71, 81, 'late.rs', 1, '', 1, 1000000)").unwrap();
            drop(conn);
            let second = fastmcp_core::block_on(select(&cx, &pool, 10, &mut position)).unwrap();
            assert_eq!(second.ids, (65..=128).collect::<Vec<_>>());
            position.applied(&second, 0);
            let last = fastmcp_core::block_on(select(&cx, &pool, 10, &mut position)).unwrap();
            assert_eq!(last.ids, vec![129]);
            assert!(last.complete);
            assert_eq!(position.ceiling, Some(201));
        });
    }
}
