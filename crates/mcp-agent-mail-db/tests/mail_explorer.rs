//! Integration tests for the unified inbox/outbox mail explorer.
//!
//! Tests the full async pipeline: seed a corpus of messages across two projects,
//! then verify direction filters, sort modes, grouping, ack-status filtering,
//! and cross-project aggregation.

#![allow(
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::missing_const_for_fn
)]

use std::collections::HashMap;

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_db::mail_explorer::{
    AckFilter, Direction, ExplorerQuery, GroupMode, SortMode, fetch_explorer_page,
};
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};

// ────────────────────────────────────────────────────────────────────
// Test harness
// ────────────────────────────────────────────────────────────────────

fn block_on<F, Fut, T>(f: F) -> T
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

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join("mail_explorer_test.db");
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 10,
        min_connections: 2,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

struct TestCorpus {
    pool: DbPool,
    /// Project IDs by name.
    projects: HashMap<&'static str, i64>,
    _dir: tempfile::TempDir,
}

fn seed_corpus(tag: &str) -> TestCorpus {
    let (pool, dir) = make_pool();

    // Create 2 projects and agents
    let (projects, agents) = {
        let p = pool.clone();
        let tag = tag.to_string();
        block_on(move |cx| async move {
            let mut projects: HashMap<&'static str, i64> = HashMap::new();
            let mut agents: HashMap<(&'static str, &'static str), i64> = HashMap::new();

            let alpha_key = format!("/tmp/__mcp-agent-mail-db-test-mail-explorer-{tag}-alpha");
            let beta_key = format!("/tmp/__mcp-agent-mail-db-test-mail-explorer-{tag}-beta");

            // Project alpha
            let proj_a = match queries::ensure_project(&cx, &p, &alpha_key).await {
                Outcome::Ok(r) => r,
                other => panic!("ensure_project alpha failed: {other:?}"),
            };
            let pid_a = proj_a.id.unwrap();
            projects.insert("alpha", pid_a);

            // Project beta
            let proj_b = match queries::ensure_project(&cx, &p, &beta_key).await {
                Outcome::Ok(r) => r,
                other => panic!("ensure_project beta failed: {other:?}"),
            };
            let pid_b = proj_b.id.unwrap();
            projects.insert("beta", pid_b);

            // Agents in project alpha
            for name in ["RedFox", "BlueLake", "GoldHawk"] {
                let agent = match queries::register_agent(
                    &cx,
                    &p,
                    pid_a,
                    name,
                    "claude-code",
                    "opus",
                    None,
                    None,
                )
                .await
                {
                    Outcome::Ok(r) => r,
                    other => panic!("register_agent({name}) failed: {other:?}"),
                };
                agents.insert((name, "alpha"), agent.id.unwrap());
            }

            // RedFox in project beta (cross-project)
            let rf_b = match queries::register_agent(
                &cx,
                &p,
                pid_b,
                "RedFox",
                "claude-code",
                "opus",
                None,
                None,
            )
            .await
            {
                Outcome::Ok(r) => r,
                other => panic!("register_agent(RedFox beta) failed: {other:?}"),
            };
            agents.insert(("RedFox", "beta"), rf_b.id.unwrap());

            // GreenMeadow in project beta
            let gm = match queries::register_agent(
                &cx,
                &p,
                pid_b,
                "GreenMeadow",
                "codex-cli",
                "o3",
                None,
                None,
            )
            .await
            {
                Outcome::Ok(r) => r,
                other => panic!("register_agent(GreenMeadow) failed: {other:?}"),
            };
            agents.insert(("GreenMeadow", "beta"), gm.id.unwrap());

            (projects, agents)
        })
    };

    let rf_a = agents[&("RedFox", "alpha")];
    let bl_a = agents[&("BlueLake", "alpha")];
    let gh_a = agents[&("GoldHawk", "alpha")];
    let rf_b = agents[&("RedFox", "beta")];
    let gm_b = agents[&("GreenMeadow", "beta")];
    let pid_a = projects["alpha"];
    let pid_b = projects["beta"];

    // Seed messages
    let p = pool.clone();
    block_on(move |cx| async move {
        // Msg 1: BlueLake → RedFox (alpha, normal, thread-1)
        match queries::create_message_with_recipients(
            &cx,
            &p,
            pid_a,
            bl_a,
            "Status update on br-42",
            "The cache layer is done. Passing all tests.",
            Some("thread-1"),
            "normal",
            false,
            "[]",
            &[(rf_a, "to")],
        )
        .await
        {
            Outcome::Ok(_) => {}
            other => panic!("msg 1 failed: {other:?}"),
        }

        // Msg 2: RedFox → BlueLake (alpha, high, thread-1, ack_required)
        match queries::create_message_with_recipients(
            &cx,
            &p,
            pid_a,
            rf_a,
            "Re: Status update on br-42",
            "Great work! Please deploy to staging.",
            Some("thread-1"),
            "high",
            true,
            "[]",
            &[(bl_a, "to")],
        )
        .await
        {
            Outcome::Ok(_) => {}
            other => panic!("msg 2 failed: {other:?}"),
        }

        // Msg 3: GoldHawk → RedFox (alpha, urgent, thread-2, ack_required)
        match queries::create_message_with_recipients(
            &cx,
            &p,
            pid_a,
            gh_a,
            "Security alert: injection vulnerability",
            "Found SQL injection in search_messages. Fix ASAP.",
            Some("thread-2"),
            "urgent",
            true,
            "[]",
            &[(rf_a, "to")],
        )
        .await
        {
            Outcome::Ok(_) => {}
            other => panic!("msg 3 failed: {other:?}"),
        }

        // Msg 4: RedFox → GoldHawk (alpha, urgent, thread-2)
        match queries::create_message_with_recipients(
            &cx,
            &p,
            pid_a,
            rf_a,
            "Re: Security alert: injection vulnerability",
            "Fixed and deployed. See commit abc123.",
            Some("thread-2"),
            "urgent",
            false,
            "[]",
            &[(gh_a, "to")],
        )
        .await
        {
            Outcome::Ok(_) => {}
            other => panic!("msg 4 failed: {other:?}"),
        }

        // Msg 5: BlueLake → RedFox + GoldHawk CC (alpha, normal, thread-3)
        match queries::create_message_with_recipients(
            &cx,
            &p,
            pid_a,
            bl_a,
            "Weekly sync notes",
            "Agenda: perf review, migration status, new beads.",
            Some("thread-3"),
            "normal",
            false,
            "[]",
            &[(rf_a, "to"), (gh_a, "cc")],
        )
        .await
        {
            Outcome::Ok(_) => {}
            other => panic!("msg 5 failed: {other:?}"),
        }

        // Msg 6: GreenMeadow → RedFox (beta, normal, cross-proj)
        match queries::create_message_with_recipients(
            &cx,
            &p,
            pid_b,
            gm_b,
            "Cross-project question about shared config",
            "Do we share the same STORAGE_ROOT between projects?",
            Some("cross-proj"),
            "normal",
            false,
            "[]",
            &[(rf_b, "to")],
        )
        .await
        {
            Outcome::Ok(_) => {}
            other => panic!("msg 6 failed: {other:?}"),
        }

        // Msg 7: RedFox → GreenMeadow (beta, normal, cross-proj)
        match queries::create_message_with_recipients(
            &cx,
            &p,
            pid_b,
            rf_b,
            "Re: Cross-project question about shared config",
            "Yes, STORAGE_ROOT is shared. Archive dirs are project-scoped.",
            Some("cross-proj"),
            "normal",
            false,
            "[]",
            &[(gm_b, "to")],
        )
        .await
        {
            Outcome::Ok(_) => {}
            other => panic!("msg 7 failed: {other:?}"),
        }

        // Msg 8: BlueLake → RedFox (alpha, low, no thread)
        match queries::create_message_with_recipients(
            &cx,
            &p,
            pid_a,
            bl_a,
            "FYI: docs updated",
            "Updated the operator runbook with new troubleshooting steps.",
            None,
            "low",
            false,
            "[]",
            &[(rf_a, "to")],
        )
        .await
        {
            Outcome::Ok(_) => {}
            other => panic!("msg 8 failed: {other:?}"),
        }
    });

    TestCorpus {
        pool,
        projects,
        _dir: dir,
    }
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[test]
fn explorer_inbound_only() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("inbound_only");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::Inbound,
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("inbound failed: {other:?}"),
        }
    });

    // RedFox receives: msg 1, 3, 5, 6, 8
    assert_eq!(
        page.entries.len(),
        5,
        "RedFox should have 5 inbound messages"
    );
    assert_eq!(page.stats.inbound_count, 5);
    assert_eq!(page.stats.outbound_count, 0);

    for e in &page.entries {
        assert_eq!(e.direction, Direction::Inbound);
    }
}

#[test]
fn explorer_outbound_only() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("outbound_only");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::Outbound,
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("outbound failed: {other:?}"),
        }
    });

    // RedFox sends: msg 2, 4, 7
    assert_eq!(
        page.entries.len(),
        3,
        "RedFox should have 3 outbound messages"
    );
    assert_eq!(page.stats.outbound_count, 3);
    assert_eq!(page.stats.inbound_count, 0);
}

#[test]
fn explorer_all_directions() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("all_directions");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::All,
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("all directions failed: {other:?}"),
        }
    });

    assert_eq!(page.entries.len(), 8);
    assert_eq!(page.stats.inbound_count, 5);
    assert_eq!(page.stats.outbound_count, 3);
}

#[test]
fn explorer_cross_project() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("cross_project");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::All,
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("cross-project failed: {other:?}"),
        }
    });

    assert!(
        page.stats.unique_projects >= 2,
        "should span at least 2 projects, got {}",
        page.stats.unique_projects
    );
}

#[test]
fn explorer_single_project_filter() {
    let TestCorpus {
        pool: p,
        projects,
        _dir,
    } = seed_corpus("single_project_filter");
    let pid = projects["alpha"];

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                project_id: Some(pid),
                direction: Direction::All,
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("single-project failed: {other:?}"),
        }
    });

    for e in &page.entries {
        assert_eq!(e.project_id, pid);
    }
    assert_eq!(page.stats.unique_projects, 1);
}

#[test]
fn explorer_sort_date_desc() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("sort_date_desc");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::All,
                sort: SortMode::DateDesc,
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("sort date desc failed: {other:?}"),
        }
    });

    for w in page.entries.windows(2) {
        assert!(w[0].created_ts >= w[1].created_ts, "should be descending");
    }
}

#[test]
fn explorer_sort_importance() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("sort_importance");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::All,
                sort: SortMode::ImportanceDesc,
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("sort importance failed: {other:?}"),
        }
    });

    assert!(!page.entries.is_empty());
    let first_imp = &page.entries[0].importance;
    assert!(
        first_imp == "urgent" || first_imp == "high",
        "first entry should be urgent/high, got {first_imp}"
    );
}

#[test]
fn explorer_group_by_project() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("group_by_project");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::All,
                group: GroupMode::Project,
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("group by project failed: {other:?}"),
        }
    });

    assert!(page.groups.len() >= 2, "should have groups for 2+ projects");
    let group_total: usize = page.groups.iter().map(|g| g.count).sum();
    assert_eq!(group_total, page.entries.len());
}

#[test]
fn explorer_group_by_thread() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("group_by_thread");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::All,
                group: GroupMode::Thread,
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("group by thread failed: {other:?}"),
        }
    });

    assert!(
        page.groups.len() >= 3,
        "should have 3+ thread groups, got {}",
        page.groups.len()
    );
}

#[test]
fn explorer_importance_filter() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("importance_filter");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::All,
                importance_filter: vec!["urgent".into(), "high".into()],
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("importance filter failed: {other:?}"),
        }
    });

    for e in &page.entries {
        assert!(
            e.importance == "urgent" || e.importance == "high",
            "expected urgent/high, got {}",
            e.importance
        );
    }
    assert!(!page.entries.is_empty());
}

#[test]
fn explorer_text_filter() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("text_filter");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::All,
                text_filter: "injection".into(),
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("text filter failed: {other:?}"),
        }
    });

    assert!(
        !page.entries.is_empty(),
        "should find messages with 'injection'"
    );
    for e in &page.entries {
        let combined = format!("{} {}", e.subject, e.body_md).to_lowercase();
        assert!(combined.contains("injection"));
    }
}

#[test]
fn explorer_pagination() {
    let TestCorpus { pool, _dir, .. } = seed_corpus("pagination");

    // Full page
    let page_full = block_on({
        let p = pool.clone();
        move |cx| async move {
            match fetch_explorer_page(
                &cx,
                &p,
                &ExplorerQuery {
                    agent_name: "RedFox".into(),
                    direction: Direction::All,
                    limit: 50,
                    ..Default::default()
                },
            )
            .await
            {
                Outcome::Ok(v) => v,
                other => panic!("full page failed: {other:?}"),
            }
        }
    });
    let total = page_full.total_count;
    assert!(total > 3);

    // Page 1
    let page1 = block_on({
        let p = pool.clone();
        move |cx| async move {
            match fetch_explorer_page(
                &cx,
                &p,
                &ExplorerQuery {
                    agent_name: "RedFox".into(),
                    direction: Direction::All,
                    limit: 3,
                    offset: 0,
                    ..Default::default()
                },
            )
            .await
            {
                Outcome::Ok(v) => v,
                other => panic!("page 1 failed: {other:?}"),
            }
        }
    });
    assert_eq!(page1.entries.len(), 3);
    assert_eq!(page1.total_count, total);

    // Page 2
    let page2 = block_on({
        let p = pool;
        move |cx| async move {
            match fetch_explorer_page(
                &cx,
                &p,
                &ExplorerQuery {
                    agent_name: "RedFox".into(),
                    direction: Direction::All,
                    limit: 3,
                    offset: 3,
                    ..Default::default()
                },
            )
            .await
            {
                Outcome::Ok(v) => v,
                other => panic!("page 2 failed: {other:?}"),
            }
        }
    });

    let p1_ids: Vec<i64> = page1.entries.iter().map(|e| e.message_id).collect();
    for e in &page2.entries {
        assert!(
            !p1_ids.contains(&e.message_id),
            "page 2 should not overlap page 1"
        );
    }
}

#[test]
fn explorer_unknown_agent_returns_empty() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("unknown_agent");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "NonExistentAgent".into(),
                direction: Direction::All,
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("unknown agent failed: {other:?}"),
        }
    });

    assert!(page.entries.is_empty());
    assert_eq!(page.total_count, 0);
}

#[test]
fn explorer_ack_filter_pending() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("ack_filter_pending");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::Inbound,
                ack_filter: AckFilter::PendingAck,
                limit: 50,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("ack filter failed: {other:?}"),
        }
    });

    for e in &page.entries {
        assert!(e.ack_required, "all entries should have ack_required=true");
    }
    assert!(!page.entries.is_empty());
}

#[test]
fn explorer_entry_fields_populated() {
    let TestCorpus { pool: p, _dir, .. } = seed_corpus("entry_fields");

    let page = block_on(move |cx| async move {
        match fetch_explorer_page(
            &cx,
            &p,
            &ExplorerQuery {
                agent_name: "RedFox".into(),
                direction: Direction::Inbound,
                limit: 1,
                sort: SortMode::DateDesc,
                ..Default::default()
            },
        )
        .await
        {
            Outcome::Ok(v) => v,
            other => panic!("entry fields failed: {other:?}"),
        }
    });

    assert_eq!(page.entries.len(), 1);
    let e = &page.entries[0];
    assert!(e.message_id > 0);
    assert!(e.project_id > 0);
    assert!(!e.project_slug.is_empty());
    assert!(!e.sender_name.is_empty());
    assert!(!e.subject.is_empty());
    assert!(e.created_ts > 0);
}
