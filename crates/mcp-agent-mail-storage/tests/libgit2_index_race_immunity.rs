//! br-8ujfs.3.8 — C8: Validate the libgit2-immunity assumption.
//!
//! Track C's migrations (C2..C6) assume libgit2 does NOT mmap `.git/index`
//! the same way CLI git does, and therefore libgit2 reads are race-immune
//! against concurrent CLI git writes. If this assumption is FALSE, all the
//! Track C migrations buy us nothing — we're just swapping one racy read
//! path for another.
//!
//! This test probes that assumption directly:
//!
//! - N reader threads inside THIS process call `git2::Repository::statuses`
//!   and walk the index in a tight loop.
//! - M writer subprocesses run real CLI git (`git add` + `git reset`) in a
//!   tight loop against the SAME repo, rewriting `.git/index` from outside.
//! - Panics inside reader threads are caught via `std::panic::catch_unwind`.
//! - If our OWN process receives SIGSEGV, the test aborts with a clear
//!   report: the hypothesis is REJECTED and Track C must be rethought.
//!
//! **Acceptance:**
//! - No SIGSEGV in any reader thread → libgit2 is immune (confirmed).
//! - Graceful libgit2 errors (`invalid index`, `corrupted data`) are
//!   ACCEPTABLE — libgit2 correctly declines to proceed when the index is
//!   momentarily inconsistent. We just need it not to crash.
//! - Zero errors is stronger confirmation; we log it but don't require it.
//!
//! **Logging:** structured stdout logs (target `libgit2_immunity`) with
//! per-agent progress and a JSON summary at end. Artifact path:
//! `tests/artifacts/libgit2_immunity/<ts>/report.json` when
//! `AM_C8_ARTIFACT_DIR` is set; otherwise the test just prints.
//!
//! **Runtime:** default 15s. `AM_C8_DURATION_SECS` overrides.
//!
//! **Opt-in gate:** this test is EXPENSIVE (spawns subprocesses) and
//! deliberately triggers CLI git segfaults on 2.51.0 boxes. Gated by
//! `AM_C8_RUN=1`. Otherwise the test is skipped with a note.

use std::panic;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

#[derive(Debug, Default)]
struct ReaderStats {
    ops_ok: AtomicU64,
    graceful_errors: AtomicU64,
    panics_caught: AtomicU64,
}

#[derive(Debug)]
struct ImmunityReport {
    mode: &'static str,
    duration_s: u64,
    readers: usize,
    writers: usize,
    reader_ops_total: u64,
    reader_errors_total: u64,
    reader_panics_total: u64,
    writer_exit_summary: Vec<WriterExit>,
    verdict: Verdict,
    git_version: String,
    started_at: String,
    ended_at: String,
}

#[derive(Debug)]
struct WriterExit {
    id: usize,
    exit_code: Option<i32>,
    signal: Option<i32>,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Zero panics, zero reader-side SIGSEGV. Hypothesis CONFIRMED.
    Immune,
    /// Graceful errors observed but no crashes. Hypothesis CONFIRMED.
    ImmuneWithGracefulErrors,
    /// At least one reader crashed. Hypothesis REJECTED.
    Racy,
}

fn iso_now() -> String {
    use chrono::Utc;
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn git_version_string() -> String {
    match Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "<unavailable>".to_string(),
    }
}

fn seed_repo(repo: &std::path::Path, file_count: usize) {
    let status = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["init", "-b", "main", "-q"])
        .status()
        .expect("git init");
    assert!(status.success());

    // Minimal identity so commits work.
    for (k, v) in [
        ("user.email", "c8@local"),
        ("user.name", "c8-immunity"),
        ("commit.gpgsign", "false"),
    ] {
        let status = Command::new("git")
            .args(["-C"])
            .arg(repo)
            .args(["config", k, v])
            .status()
            .expect("git config");
        assert!(status.success());
    }

    for i in 0..file_count {
        let p = repo.join(format!("f{i:04}.txt"));
        std::fs::write(&p, format!("content-{i}\n")).unwrap();
    }
    let status = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["add", "."])
        .status()
        .expect("git add");
    assert!(status.success());
    let status = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["commit", "-q", "-m", "seed"])
        .status()
        .expect("git commit");
    assert!(status.success());
}

fn spawn_writer_subprocess(
    repo: &std::path::Path,
    stop: Arc<AtomicBool>,
    id: usize,
) -> std::process::Child {
    // Bash loop: pick a random-ish file, re-add it, reset, repeat.
    // This forces `.git/index` rewrites. We deliberately use CLI git (not
    // libgit2) here because we're testing whether libgit2 readers survive
    // concurrent CLI writers.
    let script = format!(
        r#"
set -e
repo="$1"
idx={id}
while [ -f "$repo/.git/am-c8-running" ]; do
    # pick file index mod 100
    f="$repo/f$(printf '%04d' $((idx * 7 + RANDOM % 100))).txt"
    if [ -f "$f" ]; then
        echo "touch-$RANDOM-$idx" >> "$f"
        git -C "$repo" add "$f" >/dev/null 2>&1 || true
        git -C "$repo" reset "$f" >/dev/null 2>&1 || true
    fi
done
"#,
        id = id
    );

    // Sentinel file for cooperative shutdown.
    let sentinel = repo.join(".git/am-c8-running");
    let _ = std::fs::write(&sentinel, b"1");
    let _ = stop; // stop flag consulted by reader side; writers use sentinel

    Command::new("bash")
        .args(["-c", &script, "c8-writer"])
        .arg(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn writer subprocess")
}

fn reader_loop(repo: PathBuf, stop: Arc<AtomicBool>, stats: Arc<ReaderStats>) {
    // Each iteration: open, walk statuses + index entries. Catch any panic
    // with `catch_unwind` so a panic doesn't propagate and mask the stat
    // accounting.
    while !stop.load(Ordering::Relaxed) {
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| -> Result<(), String> {
            let repo_handle = git2::Repository::open(&repo).map_err(|e| e.to_string())?;
            // Walk statuses — exercises the index.
            let mut opts = git2::StatusOptions::new();
            opts.include_untracked(false)
                .include_ignored(false)
                .recurse_untracked_dirs(false);
            let statuses = repo_handle
                .statuses(Some(&mut opts))
                .map_err(|e| e.to_string())?;
            // Drain iterator.
            for _ in statuses.iter() {}
            // Walk the index directly.
            let index = repo_handle.index().map_err(|e| e.to_string())?;
            let _n = index.len();
            for _entry in index.iter() {}
            Ok(())
        }));

        match result {
            Ok(Ok(())) => {
                stats.ops_ok.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Err(_msg)) => {
                // Graceful libgit2 error — acceptable.
                stats.graceful_errors.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                // Panic caught — NOT acceptable (indicates libgit2 gave us
                // something bad and we panicked dereferencing it).
                stats.panics_caught.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn format_report_json(r: &ImmunityReport) -> String {
    format!(
        r#"{{
  "mode": "{mode}",
  "duration_s": {dur},
  "readers": {rd},
  "writers": {wr},
  "reader_ops_total": {ops},
  "reader_errors_total": {err},
  "reader_panics_total": {pan},
  "verdict": "{verdict:?}",
  "git_version": "{gv}",
  "started_at": "{start}",
  "ended_at": "{end}",
  "writer_exits": [{writers}]
}}"#,
        mode = r.mode,
        dur = r.duration_s,
        rd = r.readers,
        wr = r.writers,
        ops = r.reader_ops_total,
        err = r.reader_errors_total,
        pan = r.reader_panics_total,
        verdict = r.verdict,
        gv = r.git_version.replace('"', r#"\""#),
        start = r.started_at,
        end = r.ended_at,
        writers = r
            .writer_exit_summary
            .iter()
            .map(|w| format!(
                r#"{{"id":{id},"exit":{ec},"signal":{sig}}}"#,
                id = w.id,
                ec = w.exit_code.map_or("null".into(), |c| c.to_string()),
                sig = w.signal.map_or("null".into(), |s| s.to_string()),
            ))
            .collect::<Vec<_>>()
            .join(","),
    )
}

#[test]
fn libgit2_index_race_immunity() {
    if std::env::var("AM_C8_RUN").ok().as_deref() != Some("1") {
        eprintln!(
            "[C8 SKIP] AM_C8_RUN=1 not set; skipping libgit2 immunity test. \
             This test is expensive (spawns subprocesses) and deliberately \
             triggers CLI git segfaults on 2.51.0 boxes."
        );
        return;
    }

    let duration_s: u64 = std::env::var("AM_C8_DURATION_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let readers: usize = std::env::var("AM_C8_READERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let writers: usize = std::env::var("AM_C8_WRITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);

    let started_at = iso_now();
    let tmp = TempDir::new().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    seed_repo(&repo, 100);

    eprintln!(
        "[C8] started_at={started_at} duration_s={duration_s} readers={readers} writers={writers} repo={}",
        repo.display()
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(ReaderStats::default());

    // Spawn writer subprocesses (OUTSIDE our process address space).
    let mut writers_handles: Vec<std::process::Child> = (0..writers)
        .map(|id| spawn_writer_subprocess(&repo, Arc::clone(&stop), id))
        .collect();

    // Spawn reader threads (INSIDE our process — these are what we're
    // testing for crash-resistance).
    let reader_handles: Vec<_> = (0..readers)
        .map(|_| {
            let repo_clone = repo.clone();
            let stop_clone = Arc::clone(&stop);
            let stats_clone = Arc::clone(&stats);
            thread::spawn(move || {
                reader_loop(repo_clone, stop_clone, stats_clone);
            })
        })
        .collect();

    // Progress reporting every 5s so a watching operator sees something.
    let t_start = Instant::now();
    let stop_reporter = Arc::new(AtomicBool::new(false));
    let stop_reporter_thread = Arc::clone(&stop_reporter);
    let stats_reporter = Arc::clone(&stats);
    let reporter = thread::spawn(move || {
        let mut last_report = Instant::now();
        while !stop_reporter_thread.load(Ordering::Relaxed) {
            if last_report.elapsed() >= Duration::from_secs(5) {
                let ok = stats_reporter.ops_ok.load(Ordering::Relaxed);
                let errs = stats_reporter.graceful_errors.load(Ordering::Relaxed);
                let panics = stats_reporter.panics_caught.load(Ordering::Relaxed);
                eprintln!(
                    "[C8 progress] t+{}s ops_ok={ok} graceful_err={errs} panics={panics}",
                    t_start.elapsed().as_secs()
                );
                last_report = Instant::now();
            }
            thread::sleep(Duration::from_millis(250));
        }
    });

    thread::sleep(Duration::from_secs(duration_s));

    // Stop the world.
    stop.store(true, Ordering::Relaxed);
    // Signal writers via sentinel removal.
    let _ = std::fs::remove_file(repo.join(".git/am-c8-running"));

    stop_reporter.store(true, Ordering::Relaxed);
    let _ = reporter.join();

    // Join readers first — they check `stop` on every loop iteration.
    for h in reader_handles {
        let _ = h.join();
    }

    // Reap writers and record their exit states.
    let mut writer_exits: Vec<WriterExit> = Vec::with_capacity(writers);
    for (id, mut child) in writers_handles.drain(..).enumerate() {
        // Give it up to 5s, then SIGKILL.
        let child_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    #[cfg(unix)]
                    use std::os::unix::process::ExitStatusExt;
                    #[cfg(unix)]
                    let signal = status.signal();
                    #[cfg(not(unix))]
                    let signal: Option<i32> = None;
                    writer_exits.push(WriterExit {
                        id,
                        exit_code: status.code(),
                        signal,
                    });
                    break;
                }
                Ok(None) => {
                    if Instant::now() >= child_deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        writer_exits.push(WriterExit {
                            id,
                            exit_code: None,
                            signal: Some(9),
                        });
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => {
                    writer_exits.push(WriterExit {
                        id,
                        exit_code: None,
                        signal: None,
                    });
                    break;
                }
            }
        }
    }

    // Build the verdict.
    let ok_total = stats.ops_ok.load(Ordering::Relaxed);
    let err_total = stats.graceful_errors.load(Ordering::Relaxed);
    let panic_total = stats.panics_caught.load(Ordering::Relaxed);
    let verdict = if panic_total > 0 {
        Verdict::Racy
    } else if err_total > 0 {
        Verdict::ImmuneWithGracefulErrors
    } else {
        Verdict::Immune
    };

    let ended_at = iso_now();
    let report = ImmunityReport {
        mode: "mixed-inproc-readers-subprocess-writers",
        duration_s,
        readers,
        writers,
        reader_ops_total: ok_total,
        reader_errors_total: err_total,
        reader_panics_total: panic_total,
        writer_exit_summary: writer_exits,
        verdict,
        git_version: git_version_string(),
        started_at,
        ended_at,
    };

    let json = format_report_json(&report);
    eprintln!("\n[C8 REPORT]\n{json}\n");

    if let Ok(dir) = std::env::var("AM_C8_ARTIFACT_DIR") {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let artifact_dir = PathBuf::from(&dir).join(ts.to_string());
        let _ = std::fs::create_dir_all(&artifact_dir);
        let _ = std::fs::write(artifact_dir.join("report.json"), &json);
        eprintln!("[C8] report archived at {}", artifact_dir.display());
    }

    // HARD FAIL only on reader panics. Writer subprocess crashes (CLI git
    // 2.51.0 segfaulting) are EXPECTED and documented.
    assert_eq!(
        panic_total, 0,
        "libgit2 IMMUNITY HYPOTHESIS REJECTED: {panic_total} reader thread panics during concurrent \
         CLI git writes. Track C migrations alone may not be sufficient. See report above."
    );

    eprintln!(
        "[C8 PASS] verdict={:?} ops_ok={ok_total} graceful_errors={err_total} panics=0",
        report.verdict
    );
}
