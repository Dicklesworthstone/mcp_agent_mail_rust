//! GH#374: native TCP -> HTTP -> ordinary inbox resource -> live query-only DB.
//! The storage qualification owns the full archive convergence/integrity sweep;
//! this module adds real dispatch and cancellation evidence on that read path.

use super::*;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use mcp_agent_mail_storage::recovery::message_reconcile::database::{
    ReconcileCursor, reconcile_message_batch,
};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

const HTTP_BOUND: Duration = Duration::from_secs(40);
const WAIT_BOUND: Duration = Duration::from_secs(5);

struct NativeServer {
    runtime: Runtime,
    instance: Option<HttpServerInstance>,
    port: u16,
}

impl NativeServer {
    fn start(config: Config) -> Self {
        // The real multithread runtime continues serving while this test uses
        // blocking TCP clients, exactly as the headless supervisor does.
        let runtime = build_http_runtime().expect("native HTTP runtime");
        let (config, instance) = runtime
            .block_on(spawn_http_server_instance(&runtime.handle(), config))
            .expect("bind native HTTP server");
        Self {
            runtime,
            instance: Some(instance),
            port: config.http_port,
        }
    }

    fn stop(&mut self) -> std::io::Result<()> {
        self.instance.take().map_or(Ok(()), |instance| {
            self.runtime
                .block_on(stop_http_server_instance_with_timeouts(
                    instance, WAIT_BOUND, WAIT_BOUND, WAIT_BOUND,
                ))
        })
    }
}

impl Drop for NativeServer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn message_body(id: i64) -> String {
    format!("gh374-body-{id}:{}", "x".repeat(3 * 1024))
}

fn seed_mailbox(count: i64) -> (Config, DbPool) {
    let mut config = prepared_http_test_config();
    config.http_path = "/mcp/".into();
    config.http_rate_limit_enabled = false;
    config.http_jwt_enabled = false;
    config.http_rbac_enabled = false;
    let pool = mcp_agent_mail_db::get_or_create_pool(&DbPoolConfig::from_env())
        .expect("real live mailbox pool");
    let cx = runtime_request_cx(Budget::INFINITE);
    let conn = block_on(pool.acquire(&cx))
        .into_result()
        .expect("seed checkout");
    conn.execute_raw(
        "INSERT INTO projects(id,slug,human_key,created_at) VALUES(1,'gh374-http','/gh374-http',1); \
         INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES \
         (1,1,'BlueLake','test','test',1,1),(2,1,'GreenStone','test','test',1,1), \
         (3,1,'RedFox','test','test',1,1)",
    ).expect("seed identities");
    for first in (1..=count).step_by(128) {
        let end = (first + 127).min(count);
        let messages = (first..=end)
            .map(|id| {
                format!(
                    "({id},1,1,'gh374','subject-{id}','{}','high',1,{},'[]')",
                    message_body(id),
                    1_700_000_000_000_000_i64 + id * 1_000,
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let mut receipts = (first..=end)
            .map(|id| format!("({id},2,'to',2000000,2500000)"))
            .collect::<Vec<_>>();
        receipts.extend(
            (first..=end)
                .filter(|id| *id > count - 5)
                .map(|id| format!("({id},3,'to',NULL,NULL)")),
        );
        conn.execute_raw(&format!(
            "BEGIN; INSERT INTO messages(id,project_id,sender_id,thread_id,subject,body_md, \
             importance,ack_required,created_ts,attachments) VALUES {messages}; \
             INSERT INTO message_recipients(message_id,agent_id,kind,read_ts,ack_ts) VALUES {}; COMMIT",
            receipts.join(","),
        )).expect("seed bounded message batch");
    }
    drop(conn);
    block_on(mcp_agent_mail_db::queries::db_generation_id(&cx, &pool))
        .into_result()
        .expect("real database generation")
        .expect("file-backed generation");
    (config, pool)
}

fn assert_receipts_unchanged(pool: &DbPool, count: i64) {
    let cx = runtime_request_cx(Budget::INFINITE);
    let conn = block_on(pool.acquire(&cx))
        .into_result()
        .expect("witness checkout");
    let receipts = conn
        .query_sync(
            "SELECT message_id,agent_id,kind,read_ts,ack_ts FROM message_recipients \
         ORDER BY agent_id,message_id",
            &[],
        )
        .expect("receipt witnesses");
    assert_eq!(receipts.len(), usize::try_from(count + 5).unwrap());
    for (offset, row) in receipts.iter().enumerate() {
        let offset = i64::try_from(offset).unwrap();
        let (id, agent, read, ack) = if offset < count {
            (offset + 1, 2, Some(2_000_000_i64), Some(2_500_000_i64))
        } else {
            (offset - 4, 3, None, None)
        };
        assert_eq!(row.get_named::<i64>("message_id").unwrap(), id);
        assert_eq!(row.get_named::<i64>("agent_id").unwrap(), agent);
        assert_eq!(row.get_named::<String>("kind").unwrap(), "to");
        assert_eq!(row.get_named::<Option<i64>>("read_ts").unwrap(), read);
        assert_eq!(row.get_named::<Option<i64>>("ack_ts").unwrap(), ack);
    }
    let rows = conn
        .query_sync(
            "SELECT (SELECT COUNT(*) FROM messages) AS messages, \
         (SELECT COUNT(*) FROM agents WHERE last_active_ts != 1) AS activity",
            &[],
        )
        .unwrap();
    assert_eq!(rows[0].get_named::<i64>("messages").unwrap(), count);
    assert_eq!(rows[0].get_named::<i64>("activity").unwrap(), 0);
}

fn read_inbox(port: u16, agent: &str, bodies: bool) -> (Value, Instant, Instant) {
    let started = Instant::now();
    let deadline = started + HTTP_BOUND;
    let body = serde_json::to_vec(&json!({
        "jsonrpc":"2.0", "id":374, "method":"resources/read",
        "params":{"uri":format!(
            "resource://inbox/{agent}?project=gh374-http&limit=5&include_bodies={bodies}&format=json")}
    })).unwrap();
    let mut stream =
        TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), WAIT_BOUND)
            .expect("connect to native listener");
    stream.set_write_timeout(Some(WAIT_BOUND)).unwrap();
    write!(
        stream,
        "POST /mcp/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         Accept: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len(),
    )
    .unwrap();
    stream.write_all(&body).unwrap();
    let mut wire = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("HTTP wall deadline");
        stream.set_read_timeout(Some(remaining)).unwrap();
        let mut chunk = [0_u8; 8192];
        let n = stream.read(&mut chunk).expect("bounded HTTP response read");
        if n == 0 {
            break;
        }
        wire.extend_from_slice(&chunk[..n]);
        assert!(wire.len() <= 1024 * 1024, "unbounded inbox response");
    }
    let end = wire
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .expect("HTTP headers");
    let header = std::str::from_utf8(&wire[..end]).unwrap();
    assert_eq!(
        header.lines().next().unwrap().split_whitespace().nth(1),
        Some("200"),
        "{header}"
    );
    let length = header
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .expect("bounded JSON response Content-Length")
        .1
        .trim()
        .parse::<usize>()
        .unwrap();
    assert_eq!(length, wire.len() - end - 4, "complete HTTP response body");
    let response: Value = serde_json::from_slice(&wire[end + 4..]).expect("JSON-RPC response");
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 374);
    (response, started, Instant::now())
}

fn assert_inbox(response: &Value, agent: &str, bodies: bool, count: i64) {
    assert!(response.get("error").is_none(), "{response:#}");
    let contents = response["result"]["contents"]
        .as_array()
        .expect("resource contents");
    assert_eq!(contents.len(), 1);
    let inbox: Value = serde_json::from_str(contents[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(inbox["agent"], agent);
    assert_eq!(inbox["project"], "/gh374-http");
    assert_eq!(inbox["count"], 5);
    let messages = inbox["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    for (message, id) in messages.iter().zip((count - 4..=count).rev()) {
        assert_eq!(message["id"], id);
        assert_eq!(message["project_id"], 1);
        assert_eq!(message["sender_id"], 1);
        assert_eq!(message["thread_id"], "gh374");
        assert!(message["topic"].is_null());
        assert_eq!(message["subject"], format!("subject-{id}"));
        assert_eq!(message["importance"], "high");
        assert_eq!(message["from"], "BlueLake");
        assert_eq!(message["kind"], "to");
        assert_eq!(message["attachments"], json!([]));
        assert_eq!(message["ack_required"], true);
        assert_eq!(
            message["created_ts"],
            mcp_agent_mail_core::timestamps::micros_to_iso(1_700_000_000_000_000_i64 + id * 1_000)
        );
        if bodies {
            assert_eq!(message["body_md"], message_body(id));
        } else {
            assert!(message.get("body_md").is_none());
        }
    }
}

fn wait_for_dispatch_drain() {
    let deadline = Instant::now() + WAIT_BOUND;
    while DISPATCH_INFLIGHT.load(Ordering::Acquire) != 0
        || DISPATCH_ZOMBIES.load(Ordering::Acquire) != 0
    {
        assert!(
            Instant::now() < deadline,
            "HTTP worker retained dispatch admission"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct GitWriter(Child);
impl GitWriter {
    fn start(root: &Path) -> Self {
        let mut writer = Self(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["update-index", "--index-info"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("real Git writer"),
        );
        let deadline = Instant::now() + WAIT_BOUND;
        while !root.join(".git/index.lock").is_file() {
            assert!(
                writer.0.try_wait().unwrap().is_none(),
                "Git exited before locking"
            );
            assert!(Instant::now() < deadline, "Git lock deadline");
            std::thread::sleep(Duration::from_millis(5));
        }
        writer
    }
    fn stage(&mut self, id: usize, blob: git2::Oid) {
        let input = self.0.stdin.as_mut().unwrap();
        writeln!(input, "100644 {blob}\toperator-{id}.txt").unwrap();
        input.flush().unwrap();
    }
    fn finish(&mut self) {
        // Quiesce deferred sync while Git still owns its lock. Otherwise a
        // legitimate background refresh can race the staged-index witness.
        mcp_agent_mail_storage::stop_archive_index_sync(WAIT_BOUND);
        drop(self.0.stdin.take());
        let deadline = Instant::now() + WAIT_BOUND;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "Git writer: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "Git completion deadline");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for GitWriter {
    fn drop(&mut self) {
        mcp_agent_mail_storage::stop_archive_index_sync(WAIT_BOUND);
        drop(self.0.stdin.take());
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct StopReader<'a>(&'a AtomicBool);
impl Drop for StopReader<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn large_projection_overlaps(
    samples: &[(&str, bool, Instant, Instant)],
    repair_windows: &[(Instant, Instant)],
) -> [usize; 2] {
    [false, true].map(|bodies| {
        samples
            .iter()
            .filter(|(agent, included, read_start, read_end)| {
                *agent == "GreenStone"
                    && *included == bodies
                    && repair_windows.iter().any(|(repair_start, repair_end)| {
                        read_start < repair_end && repair_start < read_end
                    })
            })
            .count()
    })
}

#[test]
fn gh374_http_inbox_stays_responsive_during_reconciliation() {
    if run_prepared_http_test_in_child(
        "tests::issue374_http::gh374_http_inbox_stays_responsive_during_reconciliation",
    ) {
        return;
    }
    mcp_agent_mail_core::config::with_process_env_overrides_for_test(
        &[("DATABASE_POOL_SIZE", "2"), ("DATABASE_MAX_OVERFLOW", "0")],
        || {
            let large = std::env::var("AM_TEST_GH374_LARGE_MAILBOX").as_deref() == Ok("1");
            let count = if large { 25_100 } else { 600 };
            let (config, pool) = seed_mailbox(count);
            mcp_agent_mail_storage::ensure_archive(&config, "gh374-http").unwrap();
            let repo = git2::Repository::open(&config.storage_root).unwrap();
            let head_before = repo.head().unwrap().target();
            let blob = repo.blob(b"ordinary operator staging\n").unwrap();
            let mut git = GitWriter::start(&config.storage_root);
            let held_index = std::fs::read(repo.path().join("index")).unwrap();
            let mut server = NativeServer::start(config.clone());
            let stop = AtomicBool::new(false);
            let reads = AtomicU64::new(0);
            let cx = runtime_request_cx(Budget::INFINITE);
            let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            let mut repairs = 0;
            let mut repair_windows = Vec::new();
            let samples = Mutex::new(Vec::new());
            let mut passes = 0;
            let wall_deadline = Instant::now() + Duration::from_secs(5 * 60);
            std::thread::scope(|scope| {
                let reader = scope.spawn(|| {
                    let _stop_on_exit = StopReader(&stop);
                    while !stop.load(Ordering::Acquire) {
                        let n = reads.load(Ordering::Acquire);
                        let agent = if n % 2 == 0 { "GreenStone" } else { "RedFox" };
                        let bodies = n % 4 < 2;
                        let (response, start, end) = read_inbox(server.port, agent, bodies);
                        assert_inbox(&response, agent, bodies, count);
                        assert!(
                            end - start < Duration::from_secs(30),
                            "large inbox exceeded dispatch deadline"
                        );
                        samples
                            .lock()
                            .expect("HTTP samples")
                            .push((agent, bodies, start, end));
                        reads.fetch_add(1, Ordering::Release);
                    }
                });
                let _stop_on_unwind = StopReader(&stop);
                // Machine scheduling can place either projection between
                // short repair batches. Keep collecting genuine overlap,
                // without delaying requests or relaxing either assertion.
                for pass in 0..64 {
                    assert!(
                        Instant::now() < wall_deadline,
                        "HTTP/repair evidence exceeded five minutes after {passes} passes"
                    );
                    let prior = reads.load(Ordering::Acquire);
                    git.stage(pass, blob);
                    let start = Instant::now();
                    let report = reconcile_message_batch(&cx, &pool, &config, &mut cursor, &stop)
                        .expect("real reconciliation batch");
                    let end = Instant::now();
                    passes = pass + 1;
                    assert!(
                        !report.interrupted && report.deferred_conflict == 0,
                        "{report:?}"
                    );
                    assert!(report.scanned <= 32 && report.repaired <= 4);
                    assert!(report.payload_bytes <= 16 * 1024 * 1024);
                    repairs += report.repaired;
                    if report.repaired > 0 {
                        repair_windows.push((start, end));
                    }
                    let deadline = (Instant::now() + HTTP_BOUND).min(wall_deadline);
                    while reads.load(Ordering::Acquire) <= prior {
                        assert!(!stop.load(Ordering::Acquire), "HTTP reader failed");
                        assert!(Instant::now() < deadline, "reader made no progress");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    assert!(
                        git.0.try_wait().unwrap().is_none(),
                        "Git writer exited during repair"
                    );
                    assert!(repo.path().join("index.lock").is_file());
                    assert_eq!(
                        std::fs::read(repo.path().join("index")).unwrap(),
                        held_index
                    );
                    assert!(
                        Instant::now() < wall_deadline,
                        "HTTP/repair evidence exceeded five minutes after {passes} passes"
                    );
                    let overlaps = large_projection_overlaps(
                        &samples.lock().expect("HTTP samples"),
                        &repair_windows,
                    );
                    if passes >= 8
                        && reads.load(Ordering::Acquire) >= 8
                        && repairs > 0
                        && overlaps.iter().all(|count| *count > 0)
                    {
                        break;
                    }
                }
                stop.store(true, Ordering::Release);
                reader.join().expect("HTTP reader thread");
            });
            assert!(
                Instant::now() < wall_deadline,
                "HTTP/repair evidence collection exceeded five minutes"
            );
            let samples = samples.into_inner().expect("completed HTTP samples");
            let overlaps = large_projection_overlaps(&samples, &repair_windows);
            let large_overlaps = overlaps.iter().sum::<usize>();
            assert!((8..=64).contains(&passes));
            assert!(
                repairs > 0 && reads.load(Ordering::Acquire) >= 8 && large_overlaps > 0,
                "missing concurrent native read/repair evidence"
            );
            for (bodies, overlap_count) in [(false, overlaps[0]), (true, overlaps[1])] {
                assert!(
                    overlap_count > 0,
                    "large-inbox include_bodies={bodies} did not overlap a repair-positive batch"
                );
            }
            assert_ne!(
                repo.head().unwrap().target(),
                head_before,
                "repairs must reach Git history"
            );
            git.finish();
            assert!(!repo.path().join("index.lock").exists());
            let mut index = repo.index().unwrap();
            index.read(true).unwrap();
            let last_staged = format!("operator-{}.txt", passes - 1);
            assert_eq!(index.get_path(Path::new(&last_staged), 0).unwrap().id, blob);
            assert!(
                repo.head()
                    .unwrap()
                    .peel_to_commit()
                    .unwrap()
                    .tree()
                    .unwrap()
                    .get_path(Path::new(&last_staged))
                    .is_err(),
                "repair committed unrelated staging"
            );
            assert_receipts_unchanged(&pool, count);
            wait_for_dispatch_drain();
            server.stop().expect("graceful HTTP shutdown");
            println!(
                "{}",
                json!({"case":"gh374_native_http_reconciliation", "messages":count,
                "large_mailbox":large, "reads":samples.len(), "overlapping_large_reads":large_overlaps,
                "overlapping_large_metadata_reads":overlaps[0], "overlapping_large_body_reads":overlaps[1],
                "repairs":repairs, "passes":passes, "max_read_ms":samples.iter()
                    .map(|(_,_,start,end)| (*end-*start).as_millis()).max().unwrap()})
            );
        },
    );
}

#[test]
fn gh374_http_cancelled_inbox_releases_waiter_and_dispatch_permit() {
    if run_prepared_http_test_in_child(
        "tests::issue374_http::gh374_http_cancelled_inbox_releases_waiter_and_dispatch_permit",
    ) {
        return;
    }
    mcp_agent_mail_core::config::with_process_env_overrides_for_test(
        &[
            ("DATABASE_POOL_SIZE", "1"),
            ("DATABASE_MAX_OVERFLOW", "0"),
            ("DATABASE_POOL_TIMEOUT", "60"),
        ],
        || {
            let (config, pool) = seed_mailbox(8);
            let mut server = NativeServer::start(config);
            let (warm, _, _) = read_inbox(server.port, "GreenStone", true);
            assert_inbox(&warm, "GreenStone", true, 8);
            let read_config = DbPoolConfig {
                run_migrations: false,
                warmup_connections: 0,
                ..DbPoolConfig::from_env()
            };
            let read_pool =
                mcp_agent_mail_db::get_or_create_live_query_only_pool(&read_config).unwrap();
            assert_eq!(
                read_pool.engine_identity_key(),
                mcp_agent_mail_db::get_or_create_live_query_only_pool(&read_config)
                    .unwrap()
                    .engine_identity_key()
            );
            let cx = runtime_request_cx(Budget::INFINITE);
            let lease = block_on(read_pool.acquire(&cx))
                .into_result()
                .expect("hold only read connection");
            let (response, start, end) = std::thread::scope(|scope| {
                let request = scope.spawn(|| read_inbox(server.port, "GreenStone", true));
                let deadline = Instant::now() + WAIT_BOUND;
                loop {
                    read_pool.sample_pool_stats_now();
                    if mcp_agent_mail_core::global_metrics()
                        .db
                        .pool_pending_requests
                        .load()
                        > 0
                        && DISPATCH_INFLIGHT.load(Ordering::Acquire) > 0
                    {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "request never waited on the genuine read pool"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                request.join().expect("bounded HTTP request thread")
            });
            assert!(end - start >= Duration::from_secs(25) && end - start < HTTP_BOUND);
            assert!(
                response.get("result").is_none(),
                "cancelled read published partial mail: {response:#}"
            );
            let error = &response["error"];
            let message = error["message"].as_str().expect("JSON-RPC error message");
            let detail = error["data"]["error"]["data"]["error_detail"]
                .as_str()
                .unwrap_or("");
            let internal = serde_json::to_value(JsonRpcError::from(McpError::new(
                McpErrorCode::InternalError,
                "",
            )))
            .unwrap();
            let pool_error = serde_json::to_value(JsonRpcError::from(McpError::new(
                McpErrorCode::ToolExecutionError,
                "",
            )))
            .unwrap();
            let cancelled =
                serde_json::to_value(JsonRpcError::from(McpError::request_cancelled())).unwrap();
            // The pool's Cx deadline starts just before the outer dispatch
            // timer. Either genuine deadline path may win; unrelated failures
            // (including a generic DB error) do not qualify cancellation.
            assert!(
                error == &cancelled
                    || (error["code"] == internal["code"]
                        && (message == "Request cancelled"
                            || message.starts_with(
                                "Request timed out after 30s (method=resources/read)."
                            )))
                    || (error["code"] == pool_error["code"]
                        && error["data"]["error"]["type"] == "DATABASE_POOL_EXHAUSTED"
                        && detail.contains("acquire timeout:")
                        && (detail.contains(
                            "budget deadline reached before a connection was available"
                        ) || detail.contains("no connections available"))),
                "unexpected failure: {response:#}"
            );
            let deadline = Instant::now() + WAIT_BOUND;
            loop {
                read_pool.sample_pool_stats_now();
                if mcp_agent_mail_core::global_metrics()
                    .db
                    .pool_pending_requests
                    .load()
                    == 0
                    && DISPATCH_INFLIGHT.load(Ordering::Acquire) == 0
                    && DISPATCH_ZOMBIES.load(Ordering::Acquire) == 0
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "cancelled worker retained waiter or permit"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                mcp_agent_mail_core::global_metrics()
                    .db
                    .pool_active_connections
                    .load(),
                1
            );
            drop(lease); // Drain must be proven while the contended connection is still held.
            for bodies in [false, true] {
                let (response, _, _) = read_inbox(server.port, "GreenStone", bodies);
                assert_inbox(&response, "GreenStone", bodies, 8);
            }
            assert_receipts_unchanged(&pool, 8);
            wait_for_dispatch_drain();
            server.stop().expect("graceful HTTP shutdown");
            println!(
                "{}",
                json!({"case":"gh374_native_http_cancel", "timeout_ms":(end-start).as_millis(),
                "error":error, "waiter_drained_before_release":true, "permit_drained_before_release":true})
            );
        },
    );
}
