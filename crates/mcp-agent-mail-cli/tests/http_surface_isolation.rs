//! GH#184 e2e regression: a non-`/mcp` request (e.g. the `/mail` web UI, or a
//! bogus 404 path) must never wedge the MCP surface.
//!
//! On v0.3.20 a single unauthenticated `GET /mail` against a live
//! `am serve-http` permanently starved the whole HTTP runtime — accept loop,
//! timers, and every `/mcp` request — until the process was restarted. The
//! mail UI's synchronous DB work (per-request pool bootstrap on the live
//! database) ran inline on the async workers and could block indefinitely
//! while holding the global pool-registry write lock.
//!
//! This test drives the reported sequence end-to-end against a real spawned
//! `am serve-http` process:
//!   1. baseline `POST /mcp` answers,
//!   2. `GET /mail` (and a bogus 404 path) are issued,
//!   3. `POST /mcp` must still answer — including WHILE a `/mail` request is
//!      in flight — and the `/mail` request itself must complete.

#![forbid(unsafe_code)]

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Bind an ephemeral port, remember it, and release it for the server to use.
/// Racy in principle, but retried by the caller.
fn pick_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("pick a free port")
}

/// Issue one HTTP/1.1 request with `Connection: close` and read to EOF.
///
/// Returns `Some(status)` when a status line was received within `deadline`,
/// `None` on connect failure, timeout, or a malformed response. A `None` from
/// a request that should have been answered is exactly the GH#184 wedge.
fn http_status(port: u16, request_head: &str, body: &str, deadline: Duration) -> Option<u16> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).ok()?;
    stream.set_read_timeout(Some(deadline)).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let request = format!(
        "{request_head}Host: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;

    let start = Instant::now();
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // The status line is all we need; stop early once present.
                if buf.contains(&b'\n') {
                    break;
                }
            }
            Err(_) => break,
        }
        if start.elapsed() > deadline {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let status_line = text.lines().next()?;
    let mut parts = status_line.split_whitespace();
    let proto = parts.next()?;
    if !proto.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse::<u16>().ok()
}

fn post_mcp_status(port: u16, token: &str, deadline: Duration) -> Option<u16> {
    http_status(
        port,
        &format!(
            "POST /mcp HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json\r\n"
        ),
        "{}",
        deadline,
    )
}

fn get_status(port: u16, token: &str, path: &str, deadline: Duration) -> Option<u16> {
    http_status(
        port,
        &format!("GET {path} HTTP/1.1\r\nAuthorization: Bearer {token}\r\n"),
        "",
        deadline,
    )
}

fn isolated_command(am_bin: &Path, work: &Path) -> std::io::Result<Command> {
    for leaf in ["home/.config", "home/.cache", "home/.local/share", "tmp"] {
        std::fs::create_dir_all(work.join(leaf))?;
    }
    let mut command = Command::new(am_bin);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", work.join("home"))
        .env("XDG_CONFIG_HOME", work.join("home/.config"))
        .env("XDG_CACHE_HOME", work.join("home/.cache"))
        .env("XDG_DATA_HOME", work.join("home/.local/share"))
        .env("TMPDIR", work.join("tmp"))
        .env("AM_INTERFACE_MODE", "cli")
        .env("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1")
        .current_dir(work);
    Ok(command)
}

fn spawn_server(am_bin: &Path, work: &Path, port: u16, token: &str) -> std::io::Result<Child> {
    let db_path = work.join("storage.sqlite3");
    let storage_root = work.join("archive");
    std::fs::create_dir_all(&storage_root)?;
    let log = std::fs::File::create(work.join(format!("server-{port}.log")))?;
    isolated_command(am_bin, work)?
        .args(["serve-http", "--no-tui"])
        .current_dir(work)
        // Authenticate the test requests so GET /mail reaches the blocking UI
        // dispatch under test instead of passing through a fast 401 guard.
        .env("HTTP_BEARER_TOKEN", token)
        .env_remove("HTTP_JWT_ENABLED")
        .env("DATABASE_URL", format!("sqlite:///{}", db_path.display()))
        .env("STORAGE_ROOT", &storage_root)
        .env("HTTP_HOST", "127.0.0.1")
        .env("HTTP_PORT", port.to_string())
        .env("HTTP_PATH", "/mcp/")
        .env("TUI_ENABLED", "false")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_database_ready_server(
    am_bin: &Path,
    work: &Path,
    token: &str,
) -> Result<(ChildGuard, u16), String> {
    let mut failures = Vec::new();
    for _attempt in 0..3 {
        let port = pick_free_port();
        let mut guard = ChildGuard(
            spawn_server(am_bin, work, port, token)
                .map_err(|error| format!("spawn private server: {error}"))?,
        );
        let deadline = Instant::now() + Duration::from_secs(45);
        while Instant::now() < deadline
            && guard
                .0
                .try_wait()
                .map_err(|error| format!("inspect private server: {error}"))?
                .is_none()
        {
            if get_status(port, token, "/health", Duration::from_secs(2)) == Some(200) {
                return Ok((guard, port));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        drop(guard);
        failures.push(
            std::fs::read_to_string(work.join(format!("server-{port}.log")))
                .map_err(|error| format!("read private server log: {error}"))?,
        );
    }
    Err(format!("private server must become ready: {failures:?}"))
}

/// GH#184: the full reported wedge sequence, against a real server process.
#[test]
fn non_mcp_requests_do_not_wedge_the_mcp_surface() {
    let am_bin = Path::new(env!("CARGO_BIN_EXE_am"));
    let dir = tempfile::tempdir().expect("tempdir");
    let token = mcp_agent_mail_core::setup::generate_token().expect("private bearer token");

    // Spawn the server, retrying the port pick a few times in case of races.
    let mut spawned = None;
    for _attempt in 0..3 {
        let port = pick_free_port();
        let child = spawn_server(am_bin, dir.path(), port, &token).expect("spawn am serve-http");
        let mut guard = ChildGuard(child);

        // Wait for the listener (bind fast path answers /healthz early).
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut ready = false;
        while Instant::now() < deadline {
            if get_status(port, &token, "/healthz", Duration::from_secs(2)) == Some(200) {
                ready = true;
                break;
            }
            // Bail out early if the process already died (port collision).
            if let Ok(Some(_)) = guard.0.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if ready {
            spawned = Some((guard, port));
            break;
        }
    }
    let (guard, port) = spawned.expect("server did not become ready on any attempted port");

    // 1. Baseline: /mcp answers (authenticated empty JSON body → fast 4xx).
    let rejected_token = mcp_agent_mail_core::setup::generate_token().expect("other fixture token");
    assert_ne!(token, rejected_token);
    assert_eq!(
        post_mcp_status(port, &rejected_token, Duration::from_secs(30)),
        Some(401),
        "another fixture's bearer token must be rejected"
    );
    let baseline = post_mcp_status(port, &token, Duration::from_secs(30))
        .expect("baseline POST /mcp must answer before any /mail traffic");
    assert!(
        (400..500).contains(&baseline),
        "unexpected baseline /mcp status {baseline}"
    );

    // 2. Fire GET /mail and, WHILE it is potentially still in flight, demand
    //    that /mcp keeps answering. Pre-fix, the /mail request could occupy an
    //    async worker in a blocking pool bootstrap while holding the global
    //    pool-registry lock — starving every other request until restart.
    let mail_token = token.clone();
    let mail_thread =
        std::thread::spawn(move || get_status(port, &mail_token, "/mail", Duration::from_secs(90)));
    std::thread::sleep(Duration::from_millis(200));

    for attempt in 1..=3 {
        let status = post_mcp_status(port, &token, Duration::from_secs(30)).unwrap_or_else(|| {
            panic!("POST /mcp attempt {attempt} received no response while GET /mail in flight — GH#184 wedge")
        });
        assert!(
            (400..500).contains(&status),
            "unexpected /mcp status {status} on attempt {attempt}"
        );
    }

    // 3. The authenticated /mail request itself must complete (200 HTML or a
    //    bounded 503 timeout, but never a hang).
    let mail_status = mail_thread
        .join()
        .expect("mail thread panicked")
        .expect("GET /mail received no response at all — GH#184 wedge");
    assert!(
        matches!(mail_status, 200 | 503),
        "authenticated GET /mail did not reach the UI dispatch: status {mail_status}"
    );

    // 4. A bogus 404 path (the other reported trigger) must not wedge either.
    let bogus = get_status(
        port,
        &token,
        "/definitely-not-a-route",
        Duration::from_secs(30),
    )
    .expect("GET on a bogus path received no response — GH#184 wedge");
    assert_eq!(bogus, 404, "unexpected bogus-path status {bogus}");

    // 5. And /mcp still answers afterwards (the reporter's step 3/4).
    for attempt in 1..=2 {
        let status = post_mcp_status(port, &token, Duration::from_secs(30)).unwrap_or_else(|| {
            panic!(
                "POST /mcp attempt {attempt} after /mail+404 received no response — GH#184 wedge"
            )
        });
        assert!(
            (400..500).contains(&status),
            "unexpected post-mail /mcp status {status}"
        );
    }

    drop(guard);
}

#[test]
fn seeded_bench_uses_its_private_database_with_an_inherited_server_endpoint() {
    let am_bin = Path::new(env!("CARGO_BIN_EXE_am"));
    let served = tempfile::tempdir().expect("served mailbox");
    let benchmark = tempfile::tempdir().expect("benchmark workspace");
    let token = mcp_agent_mail_core::setup::generate_token().expect("private bearer token");
    let (_server, port) = spawn_database_ready_server(am_bin, served.path(), &token)
        .expect("private server must become ready");
    let fixture = benchmark.path().join("fixture");
    let endpoint = format!("http://127.0.0.1:{port}/mcp/");
    let command = || {
        let mut command = isolated_command(am_bin, benchmark.path()).unwrap();
        command
            .env(
                "DATABASE_URL",
                format!(
                    "sqlite:///{}",
                    benchmark.path().join("unused.sqlite3").display()
                ),
            )
            .env("STORAGE_ROOT", served.path().join("archive"))
            .env("HTTP_HOST", "127.0.0.1")
            .env("HTTP_PORT", port.to_string())
            .env("HTTP_PATH", "/mcp/")
            .env("HTTP_BEARER_TOKEN", &token)
            .env("AGENT_MAIL_URL", &endpoint)
            .env("AM_BENCH_WORKSPACE", &fixture);
        command
    };

    // Both inherited settings belong to the other daemon. Parent seeding must
    // avoid its archive owner lock; children must avoid its project lookup.
    let other_mailbox = command()
        .args(["agents", "list", "--project", "/tmp/bench", "--json"])
        .output()
        .unwrap();
    let other_error = String::from_utf8_lossy(&other_mailbox.stderr);
    assert!(!other_mailbox.status.success());
    assert!(other_error.contains("via server failed"), "{other_error}");
    assert!(other_error.contains("NOT_FOUND"), "{other_error}");

    let output = command()
        .args([
            "bench",
            "--filter",
            "agents_list",
            "--warmup",
            "1",
            "--runs",
            "3",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "seeded benchmark must use its own fixture: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["seed_report"]["inserted_agents"], 2);
    assert_eq!(report["seed_report"]["inserted_messages"], 60);
    assert_eq!(report["seeded_database_route"], "local");
    assert_eq!(report["failures"], serde_json::json!([]));
    assert_eq!(report["skipped"], serde_json::json!([]));
    assert_eq!(report["timings"]["agents_list"]["warmup_failures"], 0);
    assert_eq!(report["timings"]["agents_list"]["measurement_failures"], 0);
    assert_eq!(
        report["timings"]["agents_list"]["samples_seconds"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // Independently read the fixture through the ordinary local CLI route.
    let local = command()
        .env("AGENT_MAIL_URL", "http://127.0.0.1:0/mcp/")
        .env("HTTP_PORT", "0")
        .env(
            "DATABASE_URL",
            format!("sqlite:///{}", fixture.join("bench.sqlite3").display()),
        )
        .env("STORAGE_ROOT", fixture.join("archive"))
        .args(["agents", "list", "--project", "/tmp/bench", "--json"])
        .output()
        .unwrap();
    assert!(
        local.status.success(),
        "{}",
        String::from_utf8_lossy(&local.stderr)
    );
    let agents: serde_json::Value = serde_json::from_slice(&local.stdout).unwrap();
    let mut names: Vec<_> = agents
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["BlueLake", "RedFox"]);
}
