#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

use sqlmodel_core::Value;
use sqlmodel_frankensqlite::FrankenConnection;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(feature = "production-allocator")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const CHILD_ENV: &str = "GH372_PRESERVATION_CRASH_FIXTURE";
const COMMITTED_ACK: &str = "gh372-preservation-committed";
const UNCOMMITTED_ACK: &str = "gh372-preservation-uncommitted-staged";
const WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
struct Payload {
    revision: i64,
    subject: String,
    body: String,
}

fn payload(revision: i64) -> Payload {
    Payload {
        revision,
        subject: format!("revision {revision}"),
        body: format!("revision {revision}: {}", "retained body ".repeat(320)),
    }
}

fn fixture_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "am-gh372-memory-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .unwrap();
    let path = std::fs::canonicalize(root).unwrap().join("storage.sqlite3");
    eprintln!("GH372 preservation synthetic_fixture={}", path.display());
    path
}

fn assert_fixture_path(path: &Path) {
    assert!(path.is_absolute());
    assert_eq!(std::fs::canonicalize(path).unwrap().as_path(), path);
    assert!(
        path.parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("am-gh372-memory-"))
    );
}

fn open(path: &Path) -> FrankenConnection {
    let connection = FrankenConnection::open_file(path.to_str().unwrap()).unwrap();
    connection
        .execute_raw("PRAGMA busy_timeout=10000; PRAGMA autocommit_retain=OFF")
        .unwrap();
    connection
}

fn seed(path: &Path) {
    let connection = open(path);
    connection
        .execute_raw(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; \
             CREATE TABLE messages (id INTEGER PRIMARY KEY, subject TEXT NOT NULL, \
             body_md TEXT NOT NULL, revision INTEGER NOT NULL)",
        )
        .unwrap();
    let initial = payload(0);
    assert_eq!(
        connection
            .execute_sync(
                "INSERT INTO messages (id,subject,body_md,revision) VALUES (1,?,?,?)",
                &[
                    Value::Text(initial.subject),
                    Value::Text(initial.body),
                    Value::BigInt(initial.revision),
                ],
            )
            .unwrap(),
        1
    );
    connection.close_sync().unwrap();
}

fn update(connection: &FrankenConnection, revision: i64) {
    let next = payload(revision);
    assert_eq!(
        connection
            .execute_sync(
                "UPDATE messages SET subject=?, body_md=?, revision=? WHERE id=?",
                &[
                    Value::Text(next.subject),
                    Value::Text(next.body),
                    Value::BigInt(next.revision),
                    Value::BigInt(1),
                ],
            )
            .unwrap(),
        1
    );
}

fn read(connection: &FrankenConnection) -> Result<Payload, String> {
    let rows = connection
        .query_sync(
            "SELECT revision,subject,body_md FROM messages WHERE id=?",
            &[Value::BigInt(1)],
        )
        .map_err(|error| error.to_string())?;
    if rows.len() != 1 {
        return Err(format!("expected one message, got {}", rows.len()));
    }
    Ok(Payload {
        revision: rows[0]
            .get_as::<i64>(0)
            .map_err(|error| error.to_string())?,
        subject: rows[0]
            .get_as::<String>(1)
            .map_err(|error| error.to_string())?,
        body: rows[0]
            .get_as::<String>(2)
            .map_err(|error| error.to_string())?,
    })
}

fn canonical_check(path: &Path, revision: i64) {
    assert_fixture_path(path);
    let output = Command::new("python3")
        .args([
            "-I",
            "-c",
            "import pathlib,sqlite3,sys\np=pathlib.Path(sys.argv[1])\nr=int(sys.argv[2])\n\
             c=sqlite3.connect(p.as_uri()+'?mode=ro',uri=True)\n\
             assert c.execute('PRAGMA integrity_check').fetchall()==[('ok',)]\n\
             assert c.execute('PRAGMA foreign_key_check').fetchall()==[]\n\
             assert c.execute('SELECT COUNT(*) FROM messages').fetchone()==(1,)\n\
             assert c.execute('SELECT revision,subject,body_md FROM messages WHERE id=1').fetchone()==\
             (r,f'revision {r}',f'revision {r}: '+('retained body '*320))\n\
             print('stock-SQLite preservation oracle passed',sqlite3.sqlite_version)\n",
        ])
        .arg(path)
        .arg(revision.to_string())
        .output()
        .expect("run independent stock-SQLite oracle");
    assert!(
        output.status.success(),
        "stock-SQLite oracle failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprint!("{}", String::from_utf8_lossy(&output.stdout));
}

enum Reply {
    Ready(usize),
    Read(usize, Result<Payload, String>),
}

fn collect_reads(commands: &[Sender<()>], replies: &Receiver<Reply>, expected: i64) {
    for command in commands {
        command.send(()).unwrap();
    }
    let mut seen = std::collections::HashSet::new();
    for _ in commands {
        match replies.recv_timeout(WAIT).expect("reader deadline") {
            Reply::Read(index, result) => {
                assert!(seen.insert(index), "duplicate reader reply");
                assert_eq!(result.unwrap(), payload(expected));
            }
            Reply::Ready(_) => panic!("unexpected duplicate reader initialization"),
        }
    }
}

#[test]
fn four_readers_preserve_isolation_and_observe_commits() {
    let path = fixture_path("concurrent");
    seed(&path);
    std::thread::scope(|scope| {
        let writer = open(&path);
        let (replies_tx, replies_rx) = mpsc::channel();
        let mut commands = Vec::new();
        let mut workers = Vec::new();
        for index in 0..4 {
            let (command_tx, command_rx) = mpsc::channel();
            commands.push(command_tx);
            let path = &path;
            let replies_tx = replies_tx.clone();
            workers.push(scope.spawn(move || {
                let reader = open(path);
                replies_tx.send(Reply::Ready(index)).unwrap();
                while command_rx.recv().is_ok() {
                    if replies_tx.send(Reply::Read(index, read(&reader))).is_err() {
                        break;
                    }
                }
                reader.close_without_checkpoint_sync().unwrap();
            }));
        }
        drop(replies_tx);
        let mut ready = std::collections::HashSet::new();
        for _ in 0..4 {
            match replies_rx.recv_timeout(WAIT).expect("reader open deadline") {
                Reply::Ready(index) => assert!(ready.insert(index)),
                Reply::Read(_, _) => panic!("reader ran before all handles opened"),
            }
        }
        collect_reads(&commands, &replies_rx, 0);
        for revision in 1..=16 {
            writer.execute_raw("BEGIN IMMEDIATE").unwrap();
            update(&writer, revision);
            // All four queries run while the writer has an uncommitted update.
            // They must see the previous committed overflow payload coherently.
            collect_reads(&commands, &replies_rx, revision - 1);
            writer.execute_raw("COMMIT").unwrap();
            collect_reads(&commands, &replies_rx, revision);
        }
        drop(commands);
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(read(&writer).unwrap(), payload(16));
        writer.close_sync().unwrap();
    });
    canonical_check(&path, 16);
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
#[ignore = "internal child process, launched by committed_write_survives_sigkill_with_uncommitted_update"]
fn crash_writer_child() {
    let path = PathBuf::from(std::env::var_os(CHILD_ENV).expect("internal crash fixture"));
    assert_fixture_path(&path);
    let writer = open(&path);
    writer.execute_raw("PRAGMA synchronous=FULL").unwrap();
    writer.execute_raw("BEGIN IMMEDIATE").unwrap();
    update(&writer, 1);
    writer.execute_raw("COMMIT").unwrap();
    // libtest may have printed its progress prefix without a newline.
    println!("\n{COMMITTED_ACK}");
    std::io::stdout().flush().unwrap();
    writer.execute_raw("BEGIN IMMEDIATE").unwrap();
    update(&writer, 2);
    assert_eq!(read(&writer).unwrap(), payload(2));
    println!("\n{UNCOMMITTED_ACK}");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn committed_write_survives_sigkill_with_uncommitted_update() {
    let path = fixture_path("crash");
    seed(&path);
    std::thread::scope(|scope| {
        // Create this guard inside the scope: on a timeout it kills the child
        // before the scope waits for the stdout-draining thread to finish.
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "crash_writer_child",
                    "--exact",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD_ENV, &path)
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (lines_tx, lines_rx) = mpsc::channel();
        let output_reader = scope.spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if lines_tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut committed = false;
        let deadline = Instant::now() + WAIT;
        loop {
            let line = lines_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("crash child acknowledgement deadline");
            if line == COMMITTED_ACK {
                assert!(!committed, "duplicate commit acknowledgement");
                committed = true;
            }
            if line == UNCOMMITTED_ACK {
                assert!(
                    committed,
                    "uncommitted update ran before the acknowledged commit"
                );
                break;
            }
        }
        child.0.kill().expect("SIGKILL this synthetic child only");
        let status = child.0.wait().unwrap();
        assert_eq!(status.signal(), Some(9), "child must terminate via SIGKILL");
        output_reader.join().unwrap();
    });
    let recovered = open(&path);
    assert_eq!(read(&recovered).unwrap(), payload(1));
    let integrity = recovered.query_sync("PRAGMA integrity_check", &[]).unwrap();
    assert_eq!(integrity.len(), 1);
    assert_eq!(integrity[0].get_as::<String>(0).unwrap(), "ok");
    recovered.close_sync().unwrap();
    canonical_check(&path, 1);
}
