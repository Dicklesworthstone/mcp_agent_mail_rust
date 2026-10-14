//! Shared project deliveries in the explorer's existing query pipeline.
//!
//! Direct and shared inbox rows expose the same receipt columns, so counting,
//! filtering and candidate ordering cannot disagree about the delivery lane.
//! Only the selected viewer is expanded; this never creates per-agent rows.

use crate::error::DbError;
use crate::pool::SyncQuery;

/// Read the selected database's catalog once per explorer request. Only the
/// complete absence of both tables is a legacy mailbox, not a partial upgrade
/// or a broken view. Never silently hide existing delivery or receipt state.
pub(super) fn schema_available(conn: &impl SyncQuery) -> Result<bool, DbError> {
    let rows = conn
        .query_sync(
            "SELECT name, type FROM sqlite_master WHERE name COLLATE NOCASE \
             IN ('project_mailbox_deliveries', 'project_mailbox_receipts')",
            &[],
        )
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    if rows.is_empty() {
        return Ok(false);
    }
    let mut deliveries = false;
    let mut receipts = false;
    for row in rows {
        let name = row
            .get_named::<String>("name")
            .map_err(super::explorer_decode_error)?;
        let kind = row
            .get_named::<String>("type")
            .map_err(super::explorer_decode_error)?;
        if kind == "table" {
            deliveries |= name.eq_ignore_ascii_case("project_mailbox_deliveries");
            receipts |= name.eq_ignore_ascii_case("project_mailbox_receipts");
        }
    }
    if deliveries && receipts {
        Ok(true)
    } else {
        Err(DbError::Sqlite(
            "incomplete project mailbox schema; delivery and receipt tables must be repaired"
                .to_string(),
        ))
    }
}

/// The same `r` projection feeds both COUNT and page selection. Bindings ?1
/// and ?2 are the viewer and project selected by the explorer, not ambient
/// identity. Direct delivery precedence and eligibility come from the same
/// predicate used by the ordinary inbox, receipt and event readers.
pub(super) fn inbound_source(shared: bool) -> &'static str {
    if shared {
        concat!(
            "(SELECT direct.message_id, direct.agent_id, direct.kind, direct.read_ts, direct.ack_ts \
             FROM message_recipients direct WHERE direct.agent_id = ?1 \
             UNION ALL \
             SELECT d.message_id, viewer.id, 'project', pr.read_ts, pr.ack_ts \
             FROM project_mailbox_deliveries d \
             JOIN messages m ON m.id = d.message_id \
             JOIN agents viewer ON viewer.id = ?1 \
             LEFT JOIN project_mailbox_receipts pr \
                    ON pr.message_id = d.message_id AND pr.agent_id = viewer.id \
             WHERE d.project_id = ?2 AND ",
            crate::project_mailbox::visible_to_viewer_sql!(),
            ") r JOIN messages m ON m.id = r.message_id"
        )
    } else {
        "message_recipients r JOIN messages m ON m.id = r.message_id"
    }
}

pub(super) fn recipient_join(shared: bool) -> &'static str {
    if shared {
        "LEFT JOIN project_mailbox_deliveries project_delivery \
         ON project_delivery.message_id = m.id AND project_delivery.project_id = m.project_id"
    } else {
        ""
    }
}

/// Show the address, never enumerate the shared mailbox's eligible agents.
/// The caller still excludes direct BCC names from inbound projections and
/// includes them only in the sender's outbox. Keep the address in SQL so the
/// AgentAlpha candidate LIMIT uses the same value as the final merge.
pub(super) fn recipient_projection(shared: bool, inbound: bool) -> String {
    let recipient = if inbound { "mr2" } else { "mr" };
    let direct = format!(
        "COALESCE(GROUP_CONCAT(DISTINCT COALESCE(NULLIF(TRIM(a_recip.name), ''), \
         '[unknown-agent-' || {recipient}.agent_id || ']')), '')"
    );
    if !shared {
        return direct;
    }
    format!(
        "({direct}) || CASE WHEN project_delivery.message_id IS NOT NULL \
         THEN CASE WHEN ({direct}) = '' THEN '' ELSE ',' END || 'project:' || \
         COALESCE(NULLIF(TRIM(p.slug), ''), '[unknown-project-' || m.project_id || ']') \
         ELSE '' END"
    )
}

impl super::ExplorerQuery {
    /// Inspect shared-mailbox schema for connection-based explorer readers.
    /// Uses the same complete/legacy/partial distinction as the pooled API.
    ///
    /// # Errors
    /// Returns a database error for an unreadable or incomplete shared schema.
    pub fn shared_mailbox_schema_available(conn: &impl SyncQuery) -> Result<bool, DbError> {
        schema_available(conn)
    }

    /// Canonical eligibility for a read-only shared-delivery projection.
    ///
    /// The caller joins `project_mailbox_deliveries d`, `messages m`, and
    /// `agents viewer`, and separately bounds candidate IDs and viewers using
    /// bound parameters. This predicate includes direct-delivery precedence;
    /// it must be applied before receipt filtering or status aggregation.
    #[must_use]
    pub const fn shared_delivery_visibility_sql() -> &'static str {
        crate::project_mailbox::visible_to_viewer_sql!()
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use asupersync::runtime::{Runtime, RuntimeBuilder};

    fn expect_ok<T, E: std::fmt::Display>(result: Outcome<T, E>) -> T {
        match result {
            Outcome::Ok(value) => value,
            Outcome::Err(error) => panic!("fixture operation failed: {error}"),
            Outcome::Cancelled(_) => panic!("fixture operation cancelled"),
            Outcome::Panicked(_) => panic!("fixture operation panicked"),
        }
    }

    fn execute(runtime: &Runtime, pool: &DbPool, sql: &str) {
        runtime.block_on(async {
            let cx = Cx::current().expect("runtime context");
            let conn = expect_ok(pool.acquire(&cx).await);
            conn.execute_raw(sql).unwrap();
        });
    }

    fn fixture(test: impl FnOnce(&Runtime, &DbPool)) {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let runtime = RuntimeBuilder::current_thread().build().unwrap();
            let pool = crate::create_pool(&crate::DbPoolConfig {
                database_url: "sqlite:///:memory:".to_string(),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            for sql in [
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'alpha', '/alpha', 1), (2, 'beta', '/beta', 1)",
                "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES (10, 1, 'AmberFox', 'test', 'test', '', 1, 1), (11, 1, 'BlueLake', 'test', 'test', '', 1, 1), (12, 1, 'HiddenFox', 'test', 'test', '', 1, 1), (13, 1, 'LateFox', 'test', 'test', '', 10000, 10000), (14, 1, 'BlockedFox', 'test', 'test', '', 1, 1), (15, 1, 'RetiredFox', 'test', 'test', '', 1, 1), (20, 2, 'ZebraFox', 'test', 'test', '', 1, 1), (21, 2, 'BlueLake', 'test', 'test', '', 1, 1)",
                "UPDATE agents SET contact_policy = 'block_all' WHERE id = 14",
                "UPDATE agents SET retired_at = 10 WHERE id = 15",
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, thread_id, importance, ack_required, created_ts, recipients_json, attachments) VALUES (100, 1, 10, 'shared 100%_done', 'shared body', 'alpha-thread', 'high', 1, 100, '{}', '[]'), (101, 1, 10, 'direct', 'direct body', NULL, 'normal', 0, 200, '{}', '[]'), (102, 1, 10, 'both', 'both body', 'alpha-thread', 'low', 1, 300, '{}', '[]'), (200, 2, 20, 'beta shared', 'beta body', 'beta-thread', 'urgent', 1, 150, '{}', '[]'), (201, 2, 20, 'mismatched project', 'must stay hidden', NULL, 'urgent', 1, 400, '{}', '[]')",
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (100, 12, 'bcc'), (101, 11, 'to'), (102, 11, 'bcc')",
                "INSERT INTO project_mailbox_deliveries (message_id, project_id, kind, delivered_ts) VALUES (100, 1, 'to', 100), (102, 1, 'cc', 300), (200, 2, 'to', 150), (201, 1, 'to', 400)",
                "INSERT INTO project_mailbox_receipts (message_id, agent_id, read_ts, ack_ts) VALUES (100, 11, 7, 8), (102, 11, 99, 99), (200, 11, 88, 88)",
            ] {
                execute(&runtime, &pool, sql);
            }
            test(&runtime, &pool);
        });
    }

    fn query() -> ExplorerQuery {
        ExplorerQuery {
            agent_name: "BlueLake".to_string(),
            direction: Direction::Inbound,
            limit: 50,
            ..Default::default()
        }
    }

    fn page(runtime: &Runtime, pool: &DbPool, query: &ExplorerQuery) -> ExplorerPage {
        runtime.block_on(async {
            let cx = Cx::current().expect("runtime context");
            expect_ok(fetch_explorer_page(&cx, pool, query).await)
        })
    }

    fn ids(page: &ExplorerPage) -> Vec<i64> {
        page.entries.iter().map(|entry| entry.message_id).collect()
    }

    #[test]
    fn shared_and_direct_mail_use_one_cross_project_page_and_grouping() {
        fixture(|runtime, pool| {
            let mut query = query();
            query.group = GroupMode::Project;
            let all = page(runtime, pool, &query);
            assert_eq!(ids(&all), [102, 101, 200, 100]);
            assert_eq!(all.total_count, 4);
            assert_eq!(all.stats.inbound_count, 4);
            assert_eq!(all.stats.outbound_count, 0);
            assert_eq!(all.stats.unread_count, 3);
            assert_eq!(all.stats.pending_ack_count, 2);
            assert_eq!(all.groups.len(), 2);
            assert_eq!(all.groups[0].count, 3);
            assert_eq!(all.groups[1].count, 1);
            assert_eq!(all.entries[0].kind.as_deref(), Some("bcc"));
            assert_eq!(all.entries[0].ack_ts, None, "direct receipt wins");
            assert_eq!(all.entries[2].kind.as_deref(), Some("project"));
            assert_eq!(all.entries[2].read_ts, None, "another project identity's receipt is private");
            assert_eq!(all.entries[3].ack_ts, Some(8));
            query.project_id = Some(1);
            assert_eq!(ids(&page(runtime, pool, &query)), [102, 101, 100]);
            query.project_id = None;
            query.offset = 1;
            query.limit = 2;
            let middle = page(runtime, pool, &query);
            assert_eq!(ids(&middle), [101, 200]);
            assert_eq!(middle.total_count, 4);
            query.limit = 0;
            let count_only = page(runtime, pool, &query);
            assert!(count_only.entries.is_empty());
            assert_eq!(count_only.total_count, 4);
        });
    }

    #[test]
    fn shared_mail_respects_all_candidate_orders_and_filters() {
        fixture(|runtime, pool| {
            for (filter, expected) in [
                (AckFilter::Unread, vec![102, 101, 200]),
                (AckFilter::PendingAck, vec![102, 200]),
                (AckFilter::Acknowledged, vec![100]),
            ] {
                let query = ExplorerQuery { ack_filter: filter, ..query() };
                let result = page(runtime, pool, &query);
                assert_eq!(ids(&result), expected);
                assert_eq!(result.total_count, expected.len());
            }
            for (sort, expected) in [
                (SortMode::DateDesc, 102),
                (SortMode::DateAsc, 100),
                (SortMode::ImportanceDesc, 200),
                (SortMode::AgentAlpha, 102),
            ] {
                let query = ExplorerQuery { sort, limit: 1, ..query() };
                let result = page(runtime, pool, &query);
                assert_eq!(ids(&result), [expected]);
                assert_eq!(result.total_count, 4);
            }
            let query = ExplorerQuery {
                text_filter: "100%_done".to_string(),
                importance_filter: vec!["high".to_string()],
                ..query()
            };
            let result = page(runtime, pool, &query);
            assert_eq!(ids(&result), [100]);
            assert_eq!(result.total_count, 1);
        });
    }

    #[test]
    fn shared_visibility_excludes_sender_late_retired_opted_out_and_foreign_viewers() {
        fixture(|runtime, pool| {
            for agent in ["AmberFox", "ZebraFox", "LateFox", "BlockedFox", "RetiredFox"] {
                let query = ExplorerQuery { agent_name: agent.to_string(), ..query() };
                let result = page(runtime, pool, &query);
                assert!(result.entries.is_empty(), "{agent}");
                assert_eq!(result.total_count, 0, "{agent}");
            }
            let foreign = ExplorerQuery {
                agent_name: "BlueLake".to_string(), project_id: Some(2), ..query()
            };
            assert_eq!(ids(&page(runtime, pool, &foreign)), [200]);
            // A retired direct recipient still has its explicitly delivered
            // history. Shared eligibility must not erase that direct lane.
            execute(runtime, pool, "UPDATE agents SET retired_at = 10 WHERE id = 11");
            let retired = ExplorerQuery { project_id: Some(1), ..query() };
            assert_eq!(ids(&page(runtime, pool, &retired)), [102, 101]);
        });
    }

    #[test]
    fn shared_address_display_preserves_bcc_privacy_and_outbox_ordering() {
        fixture(|runtime, pool| {
            let incoming = page(runtime, pool, &query());
            let shared = incoming.entries.iter().find(|entry| entry.message_id == 100).unwrap();
            assert_eq!(shared.to_agents, "project:alpha");
            assert!(!shared.to_agents.contains("HiddenFox"));
            assert_eq!(incoming.stats.unique_agents, 3, "addresses are not agent identities");
            let outbox = page(runtime, pool, &ExplorerQuery {
                agent_name: "AmberFox".to_string(),
                direction: Direction::Outbound,
                sort: SortMode::AgentAlpha,
                limit: 1,
                ..Default::default()
            });
            assert_eq!(outbox.total_count, 3);
            assert_eq!(ids(&outbox), [101]);
            let outbox = page(runtime, pool, &ExplorerQuery {
                agent_name: "AmberFox".to_string(),
                direction: Direction::All,
                limit: 50,
                ..Default::default()
            });
            let shared = outbox.entries.iter().find(|entry| entry.message_id == 100).unwrap();
            assert_eq!(shared.to_agents, "HiddenFox,project:alpha");
            assert_eq!(outbox.stats.outbound_count, 3);
            assert_eq!(outbox.stats.inbound_count, 0);
        });
    }

    #[test]
    fn explorer_reads_never_create_receipts_or_per_agent_deliveries() {
        fixture(|runtime, pool| {
            execute(runtime, pool, "PRAGMA query_only = ON");
            assert_eq!(page(runtime, pool, &query()).total_count, 4);
            runtime.block_on(async {
                let cx = Cx::current().expect("runtime context");
                let conn = expect_ok(pool.acquire(&cx).await);
                for table in ["message_recipients", "project_mailbox_receipts"] {
                    let rows = conn.query_sync(&format!("SELECT COUNT(*) AS n FROM {table}"), &[]).unwrap();
                    assert_eq!(rows[0].get_named::<i64>("n").unwrap(), 3);
                }
            });
        });
    }

    #[test]
    fn partial_shared_schema_errors_but_complete_legacy_absence_remains_readable() {
        fixture(|runtime, pool| {
            execute(runtime, pool, "ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts");
            runtime.block_on(async {
                let cx = Cx::current().expect("runtime context");
                let result = fetch_explorer_page(&cx, pool, &query()).await;
                match result {
                    Outcome::Err(error) => assert!(error.to_string().contains("incomplete project mailbox schema")),
                    _ => panic!("partial schema must not report an empty shared inbox"),
                }
            });
            execute(runtime, pool, "ALTER TABLE project_mailbox_deliveries RENAME TO retained_deliveries");
            let legacy = page(runtime, pool, &query());
            assert_eq!(ids(&legacy), [102, 101]);
            assert_eq!(legacy.total_count, 2);
        });
    }
}
