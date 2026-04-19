#![allow(clippy::too_many_lines)]

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{
    Mutex,
    mpsc::{self, Receiver},
};
use std::thread;
use std::time::{Duration, Instant};

use asupersync::Cx;
use asupersync::runtime::{Runtime, RuntimeBuilder};
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::atc_retention::{LearningArtifactKind, retention_rule};
use mcp_agent_mail_core::config::with_process_env_overrides_for_test;
use mcp_agent_mail_core::{Config, ExperienceRow};
use mcp_agent_mail_db::atc_queries::{
    DEFAULT_ROLLUP_REFRESH_LOOKBACK_MICROS, OpenExperienceFilter, RollupEntry, SequenceRange,
    query_open_experiences, query_rollups, refresh_rollups, replay, retention_compact,
};
use mcp_agent_mail_db::now_micros;
use mcp_agent_mail_db::pool::get_or_create_pool;
use mcp_agent_mail_db::queries::{fetch_message_sent_atc_experience, fetch_open_atc_experiences};
use mcp_agent_mail_db::sqlmodel_core::Value as SqlValue;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use mcp_agent_mail_server::{atc, run_atc_resolution_sweep_for_integration_test, run_http};
use serde_json::{Value, json};

const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(20);
const SWEEP_TIMEOUT: Duration = Duration::from_secs(75);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const ACK_OVERDUE_URGENT_AGE_MICROS: i64 = 31 * 60 * 1_000_000;
const MICROS_PER_DAY: i64 = 86_400_000_000;

static ATC_LEARNING_LOOP_TEST_LOCK: Mutex<()> = Mutex::new(());

fn free_loopback_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("ephemeral port local_addr")
        .port();
    drop(listener);
    port
}

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> std::io::Result<(u16, Vec<u8>)> {
    let body = body.unwrap_or(&[]);
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if !body.is_empty() {
        request.push_str("Content-Type: application/json\r\n");
    }
    let _ = write!(request, "Content-Length: {}\r\n\r\n", body.len());

    stream.write_all(request.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Write);

    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;

    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response header terminator");
    let header = std::str::from_utf8(&response[..header_end]).expect("utf8 response header");
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("HTTP status code")
        .parse::<u16>()
        .expect("numeric HTTP status code");
    Ok((status, response[(header_end + 4)..].to_vec()))
}

fn wait_for_readiness(port: u16, server_result_rx: &Receiver<std::io::Result<()>>) {
    let deadline = Instant::now() + SERVER_READY_TIMEOUT;
    loop {
        if let Ok(result) = server_result_rx.try_recv() {
            panic!("server exited before readiness probe completed: {result:?}");
        }
        if let Ok((status, body)) = http_request(port, "GET", "/health/readiness", None)
            && status == 200
        {
            let value: Value = serde_json::from_slice(&body).expect("readiness JSON");
            if value.get("status").and_then(Value::as_str) == Some("ready") {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "server never became ready on port {port}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

#[allow(clippy::needless_pass_by_value)]
fn call_tool(port: u16, id: u64, tool_name: &str, arguments: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments,
        }
    });
    let request_body = serde_json::to_vec(&request).expect("serialize JSON-RPC request");
    let (status, body) =
        http_request(port, "POST", "/mcp/", Some(&request_body)).expect("POST /mcp/");
    assert_eq!(status, 200, "{tool_name} should return HTTP 200");
    let response: Value = serde_json::from_slice(&body).expect("JSON-RPC response");
    assert!(
        response.get("error").is_none(),
        "{tool_name} returned JSON-RPC error: {response:#}"
    );
    response
}

fn tool_payload(tool_name: &str, response: &Value) -> Value {
    let result = response.get("result").expect("tool result object");
    assert!(
        !result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "{tool_name} returned MCP error payload: {result:#}"
    );
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .expect("text content item");
    serde_json::from_str(text).expect("tool JSON payload")
}

fn get_message_sent_row(
    rt: &Runtime,
    cx: &Cx,
    pool: &DbPool,
    message_id: i64,
) -> mcp_agent_mail_core::experience::ExperienceRow {
    rt.block_on(fetch_message_sent_atc_experience(cx, pool, message_id))
        .into_result()
        .expect("fetch message_sent ATC row")
        .expect("message_sent ATC row exists")
}

fn age_message_sent_row(rt: &Runtime, cx: &Cx, pool: &DbPool, message_id: i64) {
    let row = rt
        .block_on(fetch_message_sent_atc_experience(cx, pool, message_id))
        .into_result()
        .expect("fetch row before aging")
        .expect("row to age exists");
    let aged_anchor = row
        .executed_ts_micros
        .or(row.dispatched_ts_micros)
        .unwrap_or(row.created_ts_micros)
        .saturating_sub(ACK_OVERDUE_URGENT_AGE_MICROS);
    let experience_id = row.experience_id;

    let conn = rt
        .block_on(pool.acquire(cx))
        .into_result()
        .expect("acquire DB connection for timestamp aging");
    conn.execute_sync(
        "UPDATE atc_experiences \
         SET created_ts = ?, dispatched_ts = ?, executed_ts = ? \
         WHERE experience_id = ?",
        &[
            SqlValue::BigInt(aged_anchor),
            SqlValue::BigInt(aged_anchor),
            SqlValue::BigInt(aged_anchor),
            SqlValue::BigInt(
                i64::try_from(experience_id).expect("experience id should fit signed SQL integer"),
            ),
        ],
    )
    .expect("age ATC row timestamps");
}

fn wait_for_message_outcome(
    rt: &Runtime,
    cx: &Cx,
    pool: &DbPool,
    message_id: i64,
    expected_label: &str,
) -> mcp_agent_mail_core::experience::ExperienceRow {
    let deadline = Instant::now() + SWEEP_TIMEOUT;
    loop {
        let row = rt
            .block_on(fetch_message_sent_atc_experience(cx, pool, message_id))
            .into_result()
            .expect("fetch message outcome during poll")
            .expect("message row should exist during poll");
        if row.outcome.as_ref().map(|outcome| outcome.label.as_str()) == Some(expected_label) {
            return row;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for outcome {expected_label} on message {message_id}; final_row={row:?}; open_rows={:?}",
            rt.block_on(fetch_open_atc_experiences(cx, pool, None, 20))
                .into_result()
                .expect("fetch open ATC experiences on timeout"),
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn open_rows_for_subject(
    rt: &Runtime,
    cx: &Cx,
    pool: &DbPool,
    subject: &str,
) -> Vec<ExperienceRow> {
    rt.block_on(fetch_open_atc_experiences(cx, pool, Some(subject), 20))
        .into_result()
        .expect("fetch_open_atc_experiences")
}

fn sender_observation_diagnostic(
    rt: &Runtime,
    cx: &Cx,
    pool: &DbPool,
    sender: &str,
    message_id: i64,
) -> String {
    let direct_row = rt
        .block_on(fetch_message_sent_atc_experience(cx, pool, message_id))
        .into_result();
    let sender_rows = rt
        .block_on(fetch_open_atc_experiences(cx, pool, Some(sender), 20))
        .into_result();
    let all_open_rows = rt
        .block_on(fetch_open_atc_experiences(cx, pool, None, 20))
        .into_result();

    format!(
        "message_id={message_id}; direct_row={direct_row:?}; sender_rows={sender_rows:?}; all_open_rows={all_open_rows:?}"
    )
}

fn query_open_rows_for(
    rt: &Runtime,
    cx: &Cx,
    pool: &DbPool,
    subject: &str,
    project_key: &str,
) -> Vec<ExperienceRow> {
    rt.block_on(query_open_experiences(
        cx,
        pool,
        OpenExperienceFilter {
            subject: Some(subject.to_string()),
            project_key: Some(project_key.to_string()),
            limit: Some(20),
            ..Default::default()
        },
    ))
    .into_result()
    .expect("query_open_experiences")
}

fn refresh_and_read_rollups(rt: &Runtime, cx: &Cx, pool: &DbPool) -> Vec<RollupEntry> {
    let now_micros = now_micros();
    let summary = rt
        .block_on(refresh_rollups(
            cx,
            pool,
            now_micros,
            DEFAULT_ROLLUP_REFRESH_LOOKBACK_MICROS,
        ))
        .into_result()
        .expect("refresh_rollups");
    assert!(
        summary.rows_applied > 0,
        "rollup refresh should apply at least one row"
    );

    rt.block_on(query_rollups(cx, pool))
        .into_result()
        .expect("query_rollups")
}

fn replay_rows(rt: &Runtime, cx: &Cx, pool: &DbPool) -> Vec<ExperienceRow> {
    rt.block_on(replay(
        cx,
        pool,
        SequenceRange {
            from_seq: None,
            to_seq: None,
        },
    ))
    .into_result()
    .expect("replay")
    .rows
}

fn resolved_retention_window_micros() -> i64 {
    let rule = retention_rule(LearningArtifactKind::ResolvedExperienceRows)
        .expect("resolved experience retention rule");
    i64::from(rule.compact_after_days.unwrap_or(rule.hot_days)) * MICROS_PER_DAY
}

fn context_message_id(row: &ExperienceRow) -> Option<i64> {
    row.context
        .as_ref()
        .and_then(|value| value.get("message_id"))
        .and_then(Value::as_i64)
}

fn message_ids(messages: &Value) -> Vec<i64> {
    messages
        .as_array()
        .expect("message list")
        .iter()
        .filter_map(|message| message.get("id").and_then(Value::as_i64))
        .collect()
}

fn path_string(path: &Path) -> String {
    path.to_str().expect("utf8 path").to_string()
}

#[allow(clippy::too_many_arguments)]
fn format_direct_send_message_diagnostic(
    rt: &Runtime,
    cx: &Cx,
    project_key: &str,
    sender_name: &str,
    recipient_name: &str,
    subject: &str,
    body_md: &str,
    thread_id: &str,
) -> String {
    let ctx = McpContext::new(cx.clone(), 99_001);
    match rt.block_on(mcp_agent_mail_tools::send_message(
        &ctx,
        project_key.to_string(),
        sender_name.to_string(),
        vec![recipient_name.to_string()],
        subject.to_string(),
        body_md.to_string(),
        None,
        None,
        None,
        None,
        Some("urgent".to_string()),
        Some(true),
        Some(thread_id.to_string()),
        None,
        None,
        None,
        None,
    )) {
        Ok(payload) => format!("direct send_message unexpectedly succeeded: {payload}"),
        Err(error) => {
            let detail = error
                .data
                .as_ref()
                .and_then(|data| data.get("error"))
                .and_then(|value| value.get("data"))
                .and_then(|value| value.get("error_detail"))
                .cloned()
                .unwrap_or(Value::Null);
            let error_data = error.data.clone().unwrap_or(Value::Null);
            format!(
                "code={:?}; message={}; error_detail={detail:#}; data={error_data:#}",
                error.code, error.message
            )
        }
    }
}

#[test]
fn e2e_message_learning_loop_covers_ack_and_ack_overdue_branches() {
    let _guard = ATC_LEARNING_LOOP_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().expect("tempdir");
    let storage_root = temp.path().join("storage-root");
    let project_root = temp.path().join("project-root");
    std::fs::create_dir_all(&storage_root).expect("create storage root");
    std::fs::create_dir_all(&project_root).expect("create project root");

    let port = free_loopback_port();
    // File-backed mailbox DBs intentionally reject durable ATC experience
    // writes today. Use the shared in-process in-memory pool so the live HTTP
    // server and the test assertions observe the same writable ATC store.
    let database_url = "sqlite:///:memory:".to_string();
    let storage_root_str = path_string(&storage_root);
    let project_key = path_string(&project_root);
    let port_str = port.to_string();

    with_process_env_overrides_for_test(
        &[
            ("DATABASE_URL", database_url.as_str()),
            ("STORAGE_ROOT", storage_root_str.as_str()),
            ("HTTP_HOST", "127.0.0.1"),
            ("HTTP_PORT", port_str.as_str()),
            ("HTTP_PATH", "/mcp/"),
            ("TUI_ENABLED", "false"),
            ("HTTP_RATE_LIMIT_ENABLED", "false"),
            ("HTTP_RBAC_ENABLED", "false"),
            ("HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED", "true"),
            ("AM_ATC_WRITE_MODE", "live"),
            ("DATABASE_POOL_SIZE", "1"),
            ("DATABASE_MAX_OVERFLOW", "0"),
            // The operator loop clamps probe cadence to at least 5s and only
            // runs the open-experience sweep every 10 ticks, so the overdue
            // branch can legitimately take about 50s to resolve.
            ("AM_ATC_PROBE_INTERVAL_SECS", "1"),
        ],
        || {
            Config::reset_cached();

            let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
            let cx = Cx::for_testing();
            let server_config = Config::from_env();
            let (server_result_tx, server_result_rx) = mpsc::channel();
            thread::spawn(move || {
                let result = run_http(&server_config);
                let _ = server_result_tx.send(result);
            });

            wait_for_readiness(port, &server_result_rx);
            let db_pool = get_or_create_pool(&DbPoolConfig::from_env()).expect("shared test pool");

            let ensure_project = tool_payload(
                "ensure_project",
                &call_tool(
                    port,
                    1,
                    "ensure_project",
                    json!({ "human_key": project_key }),
                ),
            );
            assert_eq!(
                ensure_project
                    .get("human_key")
                    .and_then(Value::as_str)
                    .expect("project human_key"),
                project_key
            );

            let alpha = tool_payload(
                "register_agent.alpha",
                &call_tool(
                    port,
                    2,
                    "register_agent",
                    json!({
                        "project_key": project_key,
                        "program": "codex-cli",
                        "model": "gpt-5",
                        "name": "BlueHarbor",
                        "task_description": "ATC learning loop sender"
                    }),
                ),
            );
            assert_eq!(
                alpha
                    .get("name")
                    .and_then(Value::as_str)
                    .expect("BlueHarbor name"),
                "BlueHarbor"
            );

            let beta = tool_payload(
                "register_agent.beta",
                &call_tool(
                    port,
                    3,
                    "register_agent",
                    json!({
                        "project_key": project_key,
                        "program": "codex-cli",
                        "model": "gpt-5",
                        "name": "GreenCanyon",
                        "task_description": "ATC learning loop recipient"
                    }),
                ),
            );
            assert_eq!(
                beta.get("name")
                    .and_then(Value::as_str)
                    .expect("GreenCanyon name"),
                "GreenCanyon"
            );

            let recipient_policy = tool_payload(
                "set_contact_policy.recipient",
                &call_tool(
                    port,
                    4,
                    "set_contact_policy",
                    json!({
                        "project_key": project_key,
                        "agent_name": "GreenCanyon",
                        "policy": "open"
                    }),
                ),
            );
            assert_eq!(
                recipient_policy
                    .get("agent")
                    .and_then(Value::as_str)
                    .expect("GreenCanyon contact policy agent"),
                "GreenCanyon"
            );
            assert_eq!(
                recipient_policy
                    .get("policy")
                    .and_then(Value::as_str)
                    .expect("GreenCanyon contact policy"),
                "open"
            );

            let sent_one_response = call_tool(
                port,
                5,
                "send_message",
                json!({
                    "project_key": project_key,
                    "sender_name": "BlueHarbor",
                    "to": ["GreenCanyon"],
                    "subject": "Loop message one",
                    "body_md": "Ack this one.",
                    "thread_id": "br-bn0vb.14",
                    "ack_required": true,
                    "importance": "urgent"
                }),
            );
            if sent_one_response
                .get("result")
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let diagnostic = format_direct_send_message_diagnostic(
                    &runtime,
                    &cx,
                    &project_key,
                    "BlueHarbor",
                    "GreenCanyon",
                    "Loop message one",
                    "Ack this one.",
                    "br-bn0vb.14",
                );
                panic!(
                    "send_message.one returned MCP error payload: {:#}\ndirect diagnostic: {diagnostic}",
                    sent_one_response
                        .get("result")
                        .expect("send_message.one result payload")
                );
            }
            let sent_one = tool_payload("send_message.one", &sent_one_response);
            let message_one_id = sent_one
                .get("deliveries")
                .and_then(Value::as_array)
                .and_then(|deliveries| deliveries.first())
                .and_then(|delivery| delivery.get("payload"))
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_i64)
                .expect("message one id");

            let alpha_open_after_send =
                open_rows_for_subject(&runtime, &cx, &db_pool, "BlueHarbor");
            assert_eq!(
                alpha_open_after_send.len(),
                1,
                "{}",
                sender_observation_diagnostic(
                    &runtime,
                    &cx,
                    &db_pool,
                    "BlueHarbor",
                    message_one_id,
                )
            );
            assert_eq!(alpha_open_after_send[0].decision_class, "message_sent");

            let inbox_one = tool_payload(
                "fetch_inbox.one",
                &call_tool(
                    port,
                    6,
                    "fetch_inbox",
                    json!({
                        "project_key": project_key,
                        "agent_name": "GreenCanyon"
                    }),
                ),
            );
            assert!(
                message_ids(&inbox_one).contains(&message_one_id),
                "GreenCanyon inbox should include message one"
            );

            let beta_open_after_receive =
                open_rows_for_subject(&runtime, &cx, &db_pool, "GreenCanyon");
            assert_eq!(
                beta_open_after_receive.len(),
                1,
                "receiver rows after first fetch_inbox: {beta_open_after_receive:#?}"
            );
            assert_eq!(
                beta_open_after_receive[0].decision_class,
                "message_received"
            );

            let ack_one = tool_payload(
                "acknowledge_message.one",
                &call_tool(
                    port,
                    7,
                    "acknowledge_message",
                    json!({
                        "project_key": project_key,
                        "agent_name": "GreenCanyon",
                        "message_id": message_one_id
                    }),
                ),
            );
            assert_eq!(
                ack_one
                    .get("message_id")
                    .and_then(Value::as_i64)
                    .expect("acknowledged message id"),
                message_one_id
            );

            let acknowledged_row = get_message_sent_row(&runtime, &cx, &db_pool, message_one_id);
            assert_eq!(acknowledged_row.state.to_string(), "resolved");
            assert_eq!(
                acknowledged_row
                    .outcome
                    .as_ref()
                    .map(|outcome| outcome.label.as_str()),
                Some("acknowledged")
            );
            // The ATC summary reports controller-level liveness/calibration
            // state rather than the DB-backed message learning rows validated
            // in this test, so avoid timing-sensitive assertions on its
            // experience counters here.
            let _summary_after_ack = atc::atc_summary().expect("ATC summary after ack");

            let alpha_open_after_ack = open_rows_for_subject(&runtime, &cx, &db_pool, "BlueHarbor");
            assert!(
                alpha_open_after_ack.is_empty(),
                "acknowledged send experience should leave no sender-side open rows; {}",
                sender_observation_diagnostic(
                    &runtime,
                    &cx,
                    &db_pool,
                    "BlueHarbor",
                    message_one_id,
                )
            );

            let sent_two = tool_payload(
                "send_message.two",
                &call_tool(
                    port,
                    8,
                    "send_message",
                    json!({
                        "project_key": project_key,
                        "sender_name": "BlueHarbor",
                        "to": ["GreenCanyon"],
                        "subject": "Loop message two",
                        "body_md": "Do not acknowledge this one.",
                        "thread_id": "br-bn0vb.14",
                        "ack_required": true,
                        "importance": "urgent"
                    }),
                ),
            );
            let message_two_id = sent_two
                .get("deliveries")
                .and_then(Value::as_array)
                .and_then(|deliveries| deliveries.first())
                .and_then(|delivery| delivery.get("payload"))
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_i64)
                .expect("message two id");

            let alpha_open_before_overdue =
                open_rows_for_subject(&runtime, &cx, &db_pool, "BlueHarbor");
            assert_eq!(
                alpha_open_before_overdue.len(),
                1,
                "{}",
                sender_observation_diagnostic(
                    &runtime,
                    &cx,
                    &db_pool,
                    "BlueHarbor",
                    message_two_id,
                )
            );
            assert_eq!(alpha_open_before_overdue[0].decision_class, "message_sent");

            let inbox_two = tool_payload(
                "fetch_inbox.two",
                &call_tool(
                    port,
                    9,
                    "fetch_inbox",
                    json!({
                        "project_key": project_key,
                        "agent_name": "GreenCanyon"
                    }),
                ),
            );
            assert!(
                message_ids(&inbox_two).contains(&message_two_id),
                "GreenCanyon inbox should include message two"
            );

            age_message_sent_row(&runtime, &cx, &db_pool, message_two_id);
            // The operator thread's periodic cadence is covered in the server
            // unit tests. Drive the same sweep directly here so this E2E stays
            // deterministic while still validating the end-to-end learning
            // pipeline from tool call to resolved ATC snapshot.
            run_atc_resolution_sweep_for_integration_test(&db_pool, now_micros(), 600_000_000);

            let overdue_row =
                wait_for_message_outcome(&runtime, &cx, &db_pool, message_two_id, "ack_overdue");
            assert_eq!(overdue_row.state.to_string(), "resolved");

            let alpha_open_after_overdue =
                open_rows_for_subject(&runtime, &cx, &db_pool, "BlueHarbor");
            assert!(
                alpha_open_after_overdue.is_empty(),
                "ack-overdue sweep should resolve sender-side open experience; {}",
                sender_observation_diagnostic(
                    &runtime,
                    &cx,
                    &db_pool,
                    "BlueHarbor",
                    message_two_id,
                )
            );

            let beta_open_after_two_receives =
                open_rows_for_subject(&runtime, &cx, &db_pool, "GreenCanyon");
            assert!(
                beta_open_after_two_receives.is_empty(),
                "receiver-side message_received rows should resolve after the sweep observes GreenCanyon activity: {beta_open_after_two_receives:#?}"
            );

            let rollups = refresh_and_read_rollups(&runtime, &cx, &db_pool);
            let synthesis_advisory = rollups
                .iter()
                .find(|entry| entry.subsystem == "synthesis" && entry.effect_kind == "advisory")
                .expect("synthesis/advisory rollup entry");
            let normalized_project_key = mcp_agent_mail_db::queries::generate_slug(&project_key);
            let receiver_open_rows = query_open_rows_for(
                &runtime,
                &cx,
                &db_pool,
                "GreenCanyon",
                &normalized_project_key,
            );
            let replay_rows = replay_rows(&runtime, &cx, &db_pool);
            let retained = runtime
                .block_on(retention_compact(
                    &cx,
                    &db_pool,
                    resolved_retention_window_micros(),
                    true,
                ))
                .into_result()
                .expect("retention_compact should preserve recent rows");

            assert!(synthesis_advisory.total_count >= 2);
            assert!(synthesis_advisory.resolved_count >= 2);
            assert!(synthesis_advisory.correct_count >= 1);
            assert!(synthesis_advisory.incorrect_count >= 1);
            assert!(receiver_open_rows.is_empty());
            assert_eq!(retained.deleted_rows, 0);
            assert!(retained.preserved_rollups);
            assert!(
                replay_rows.iter().any(|row| {
                    row.decision_class == "message_sent"
                        && context_message_id(row) == Some(message_one_id)
                }),
                "replay should include the acknowledged sender experience"
            );
            assert!(
                replay_rows.iter().any(|row| {
                    row.decision_class == "message_sent"
                        && context_message_id(row) == Some(message_two_id)
                }),
                "replay should include the ack-overdue sender experience"
            );
            assert_eq!(
                replay_rows
                    .iter()
                    .filter(|row| row.decision_class == "message_received")
                    .count(),
                2,
                "replay should preserve both receiver-side open rows"
            );
        },
    );

    Config::reset_cached();
}
