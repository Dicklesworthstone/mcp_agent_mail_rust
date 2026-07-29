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

const TEST_BEARER_TOKEN: &str = "gh184-http-surface-isolation-token";

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

fn post_mcp_status(port: u16, deadline: Duration) -> Option<u16> {
    http_status(
        port,
        &format!(
            "POST /mcp HTTP/1.1\r\nAuthorization: Bearer {TEST_BEARER_TOKEN}\r\nContent-Type: application/json\r\nAccept: application/json\r\n"
        ),
        "{}",
        deadline,
    )
}

fn get_status(port: u16, path: &str, deadline: Duration) -> Option<u16> {
    http_status(
        port,
        &format!("GET {path} HTTP/1.1\r\nAuthorization: Bearer {TEST_BEARER_TOKEN}\r\n"),
        "",
        deadline,
    )
}

fn spawn_server(am_bin: &Path, work: &Path, port: u16) -> std::io::Result<Child> {
    let db_path = work.join("storage.sqlite3");
    let storage_root = work.join("archive");
    std::fs::create_dir_all(&storage_root)?;
    Command::new(am_bin)
        .args(["serve-http", "--no-tui"])
        .current_dir(work)
        // Authenticate the test requests so GET /mail reaches the blocking UI
        // dispatch under test instead of passing through a fast 401 guard.
        .env("HTTP_BEARER_TOKEN", TEST_BEARER_TOKEN)
        .env_remove("HTTP_JWT_ENABLED")
        .env("DATABASE_URL", format!("sqlite:///{}", db_path.display()))
        .env("STORAGE_ROOT", &storage_root)
        .env("HTTP_HOST", "127.0.0.1")
        .env("HTTP_PORT", port.to_string())
        .env("TUI_ENABLED", "false")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// GH#184: the full reported wedge sequence, against a real server process.
#[test]
fn non_mcp_requests_do_not_wedge_the_mcp_surface() {
    let am_bin = Path::new(env!("CARGO_BIN_EXE_am"));
    let dir = tempfile::tempdir().expect("tempdir");

    // Spawn the server, retrying the port pick a few times in case of races.
    let mut spawned = None;
    for _attempt in 0..3 {
        let port = pick_free_port();
        let child = spawn_server(am_bin, dir.path(), port).expect("spawn am serve-http");
        let mut guard = ChildGuard(child);

        // Wait for the listener (bind fast path answers /healthz early).
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut ready = false;
        while Instant::now() < deadline {
            if get_status(port, "/healthz", Duration::from_secs(2)) == Some(200) {
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
    let baseline = post_mcp_status(port, Duration::from_secs(30))
        .expect("baseline POST /mcp must answer before any /mail traffic");
    assert!(
        (400..500).contains(&baseline),
        "unexpected baseline /mcp status {baseline}"
    );

    // 2. Fire GET /mail and, WHILE it is potentially still in flight, demand
    //    that /mcp keeps answering. Pre-fix, the /mail request could occupy an
    //    async worker in a blocking pool bootstrap while holding the global
    //    pool-registry lock — starving every other request until restart.
    let mail_thread =
        std::thread::spawn(move || get_status(port, "/mail", Duration::from_secs(90)));
    std::thread::sleep(Duration::from_millis(200));

    for attempt in 1..=3 {
        let status = post_mcp_status(port, Duration::from_secs(30)).unwrap_or_else(|| {
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
    let bogus = get_status(port, "/definitely-not-a-route", Duration::from_secs(30))
        .expect("GET on a bogus path received no response — GH#184 wedge");
    assert_eq!(bogus, 404, "unexpected bogus-path status {bogus}");

    // 5. And /mcp still answers afterwards (the reporter's step 3/4).
    for attempt in 1..=2 {
        let status = post_mcp_status(port, Duration::from_secs(30)).unwrap_or_else(|| {
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
