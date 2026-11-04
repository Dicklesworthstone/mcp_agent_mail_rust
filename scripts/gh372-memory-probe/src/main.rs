#![forbid(unsafe_code)]

use fsqlite::{AsyncConnection, Connection, SqliteValue};
use futures_lite::future::block_on;
use sqlmodel_core::Value;
use sqlmodel_frankensqlite::FrankenConnection;
use std::path::{Path, PathBuf};

#[cfg(feature = "production-allocator")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Clone, Copy, Debug)]
enum Backend {
    Adapter,
    Async,
    Raw,
}

enum Handle {
    Adapter(FrankenConnection),
    Async(AsyncConnection),
    Raw(Box<Connection>),
}

#[derive(Debug, PartialEq, Eq)]
enum Cell {
    Integer(i64),
    Text(String),
}

type Rows = Vec<Vec<Cell>>;

fn engine_rows(rows: Vec<fsqlite::Row>) -> Rows {
    rows.iter()
        .map(|row| {
            row.values()
                .iter()
                .map(|cell| match cell {
                    SqliteValue::Integer(value) => Cell::Integer(*value),
                    SqliteValue::Text(value) => Cell::Text(value.to_string()),
                    other => panic!("unexpected diagnostic value: {other:?}"),
                })
                .collect()
        })
        .collect()
}

// Keep the enormous raw futures out of every adapter/AsyncConnection stack
// frame. A branch in an unexecuted match arm can still enlarge that function's
// stack frame; separate non-inlined functions avoid contaminating the probe.
#[inline(never)]
fn raw_open(path: &str) -> Box<Connection> {
    Box::new(block_on(Connection::open(path)).unwrap())
}

#[inline(never)]
fn raw_execute(conn: &Connection, sql: &str) {
    block_on(conn.execute(sql)).unwrap();
}

#[inline(never)]
fn raw_query(conn: &Connection, sql: &str, params: &[SqliteValue]) -> Vec<fsqlite::Row> {
    if params.is_empty() {
        block_on(conn.query(sql))
    } else {
        block_on(conn.query_with_params(sql, params))
    }
    .unwrap()
}

#[inline(never)]
fn raw_close(conn: Connection) {
    block_on(conn.close()).unwrap();
}

impl Handle {
    #[inline(never)]
    fn open(backend: Backend, path: &str) -> Self {
        match backend {
            Backend::Adapter => Self::Adapter(FrankenConnection::open_file(path).unwrap()),
            Backend::Async => Self::Async(AsyncConnection::open_sync(path).unwrap()),
            Backend::Raw => Self::Raw(raw_open(path)),
        }
    }

    #[inline(never)]
    fn execute(&self, sql: &str) {
        match self {
            Self::Adapter(conn) => conn.execute_raw(sql).unwrap(),
            Self::Async(conn) => {
                conn.execute_sync(sql).unwrap();
            }
            Self::Raw(conn) => raw_execute(conn, sql),
        }
    }

    #[inline(never)]
    fn query(&self, sql: &str, params: &[i64]) -> Rows {
        match self {
            Self::Adapter(conn) => {
                let params: Vec<_> = params.iter().copied().map(Value::BigInt).collect();
                conn.query_sync(sql, &params)
                    .unwrap()
                    .into_iter()
                    .map(|row| {
                        (0..row.len())
                            .map(|index| match row.get(index).unwrap() {
                                Value::Int(value) => Cell::Integer(i64::from(*value)),
                                Value::BigInt(value) => Cell::Integer(*value),
                                Value::Text(value) => Cell::Text(value.clone()),
                                other => panic!("unexpected diagnostic value: {other:?}"),
                            })
                            .collect()
                    })
                    .collect()
            }
            Self::Async(conn) => {
                let params: Vec<_> = params.iter().copied().map(SqliteValue::Integer).collect();
                let rows = if params.is_empty() {
                    conn.query_sync(sql)
                } else {
                    conn.query_with_params_sync(sql, &params)
                }
                .unwrap();
                engine_rows(rows)
            }
            Self::Raw(conn) => {
                let params: Vec<_> = params.iter().copied().map(SqliteValue::Integer).collect();
                let rows = raw_query(conn, sql, &params);
                engine_rows(rows)
            }
        }
    }

    #[inline(never)]
    fn close(self) {
        match self {
            Self::Adapter(conn) => conn.close_sync().unwrap(),
            Self::Async(mut conn) => conn.close_sync().unwrap(),
            Self::Raw(conn) => raw_close(*conn),
        }
    }
}

#[derive(Default, Debug)]
struct Mapping {
    header: String,
    size: u64,
    rss: u64,
    pss: u64,
    anonymous: u64,
    private_dirty: u64,
    file_backed: bool,
}

fn number(line: &str) -> u64 {
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn report_map(stage: &str, mapping: &Mapping, detail: bool) {
    if detail
        && (mapping.rss >= 256
            || (!mapping.file_backed && mapping.size >= 32768)
            || mapping.header.contains(".sqlite3"))
    {
        eprintln!(
            "MAP stage={stage} size_kib={} rss_kib={} pss_kib={} anon_kib={} private_dirty_kib={} {}",
            mapping.size,
            mapping.rss,
            mapping.pss,
            mapping.anonymous,
            mapping.private_dirty,
            mapping.header
        );
    }
}

fn status_field(status: &str, name: &str) -> u64 {
    status
        .lines()
        .find(|line| line.starts_with(name))
        .map(number)
        .unwrap_or(0)
}

fn report_worker_stacks(stage: &str) {
    let tasks = match std::fs::read_dir("/proc/self/task") {
        Ok(tasks) => tasks,
        Err(error) => {
            eprintln!("WORKER_STACKS stage={stage} unavailable={error:?}");
            return;
        }
    };
    for task in tasks.flatten() {
        let path = task.path();
        let Ok(name) = std::fs::read_to_string(path.join("comm")) else {
            continue;
        };
        if name.trim() != "fsqlite-worker" {
            continue;
        }
        let tid = task.file_name();
        let tid = tid.to_string_lossy();
        match std::fs::read_to_string(path.join("syscall")) {
            Ok(syscall) if syscall.trim() == "running" => {
                eprintln!("WORKER_STACK stage={stage} tid={tid} state=running");
            }
            Ok(syscall) => {
                // Linux reports the stack pointer immediately before the final
                // instruction pointer, including the three-field -1 form.
                let fields: Vec<_> = syscall.split_whitespace().collect();
                if fields.len() >= 3 {
                    eprintln!(
                        "WORKER_STACK stage={stage} tid={tid} stack_pointer={} syscall={}",
                        fields[fields.len() - 2],
                        fields[0]
                    );
                } else {
                    eprintln!(
                        "WORKER_STACK stage={stage} tid={tid} unavailable=unrecognized_syscall_format"
                    );
                }
            }
            Err(error) => {
                eprintln!("WORKER_STACK stage={stage} tid={tid} unavailable={error:?}");
            }
        }
    }
}

fn snapshot(stage: &str, detailed: bool) -> (u64, u64) {
    // Read status before diagnostic allocations. VmHWM is lifetime-wide and
    // intentionally retained: this probe never resets it or changes the budget.
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let rss = status_field(&status, "VmRSS:");
    let peak = status_field(&status, "VmHWM:");
    eprintln!(
        "MEM stage={stage} rss_kib={rss} peak_kib={peak} size_kib={} anon_kib={} file_kib={} shmem_kib={} threads={}",
        status_field(&status, "VmSize:"),
        status_field(&status, "RssAnon:"),
        status_field(&status, "RssFile:"),
        status_field(&status, "RssShmem:"),
        status_field(&status, "Threads:")
    );
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap();
    eprintln!(
        "ROLLUP stage={stage} rss_kib={} pss_kib={} anonymous_kib={} private_dirty_kib={} shared_clean_kib={}",
        status_field(&rollup, "Rss:"),
        status_field(&rollup, "Pss:"),
        status_field(&rollup, "Anonymous:"),
        status_field(&rollup, "Private_Dirty:"),
        status_field(&rollup, "Shared_Clean:")
    );
    if detailed {
        let maps = std::fs::read_to_string("/proc/self/smaps").unwrap();
        let mut mapping = Mapping::default();
        for line in maps.lines() {
            if line
                .split_whitespace()
                .next()
                .is_some_and(|word| word.contains('-'))
            {
                report_map(stage, &mapping, detailed);
                mapping = Mapping {
                    header: line.to_string(),
                    file_backed: line.contains('/'),
                    ..Mapping::default()
                };
            } else if line.starts_with("Size:") {
                mapping.size = number(line);
            } else if line.starts_with("Rss:") {
                mapping.rss = number(line);
            } else if line.starts_with("Pss:") {
                mapping.pss = number(line);
            } else if line.starts_with("Anonymous:") {
                mapping.anonymous = number(line);
            } else if line.starts_with("Private_Dirty:") {
                mapping.private_dirty = number(line);
            }
        }
        report_map(stage, &mapping, detailed);
        report_worker_stacks(stage);
    }
    (rss, peak)
}

fn validate_fixture(path: &Path) -> PathBuf {
    assert!(path.is_absolute(), "fixture path must be absolute");
    let canonical = std::fs::canonicalize(path).unwrap();
    assert_eq!(path, canonical, "fixture must have no symlink aliases");
    assert!(
        canonical
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("am-gh372-memory-")),
        "this diagnostic is restricted to synthetic GH372 fixtures"
    );
    assert!(canonical.is_file());
    canonical
}

#[inline(never)]
fn run(path: PathBuf, count: usize, backend: Backend, workload: String, detailed: bool) {
    let path = path.to_str().unwrap();
    eprintln!(
        "CONFIG backend={backend:?} connections={count} workload={workload} allocator={} file_bytes={}",
        if cfg!(feature = "production-allocator") {
            "mimalloc"
        } else {
            "system"
        },
        std::fs::metadata(path).unwrap().len()
    );
    let (baseline, initial_peak) = snapshot("baseline", detailed);
    let mut connections = Vec::with_capacity(count);
    for index in 0..count {
        connections.push(Handle::open(backend, path));
        snapshot(&format!("opened_{}", index + 1), detailed);
    }
    if workload != "open" {
        if workload == "mailbox" {
            connections[0]
                .execute("UPDATE messages SET subject = 'post-upgrade write' WHERE id = 1");
        } else if workload == "metrics" {
            connections[0].execute("UPDATE tool_metrics_snapshots SET calls = 372 WHERE id = 1");
        } else {
            assert_eq!(workload, "select");
        }
        snapshot("after_write", detailed);
        let readers = if count == 1 {
            &connections[..]
        } else {
            &connections[1..]
        };
        for (index, reader) in readers.iter().enumerate() {
            assert_eq!(reader.query("SELECT 1", &[]), vec![vec![Cell::Integer(1)]]);
            if workload == "mailbox" {
                let rows = reader.query("SELECT subject, body_md FROM messages WHERE id = ?", &[1]);
                assert_eq!(
                    rows,
                    vec![vec![
                        Cell::Text("post-upgrade write".into()),
                        Cell::Text("retained body ".repeat(320))
                    ]]
                );
            } else if workload == "metrics" {
                let rows = reader.query(
                    "SELECT calls FROM tool_metrics_snapshots WHERE id = ?",
                    &[1],
                );
                assert_eq!(rows, vec![vec![Cell::Integer(372)]]);
            }
            snapshot(&format!("read_{}", index + 1), detailed);
        }
    }
    let (final_rss, peak) = snapshot("measured", detailed);
    eprintln!(
        "RESULT baseline_kib={baseline} initial_peak_kib={initial_peak} final_rss_kib={final_rss} peak_kib={peak} delta_kib={} live_delta_kib={}",
        peak.saturating_sub(baseline),
        final_rss.saturating_sub(baseline)
    );
    // Diagnostic queries follow the measurement so they cannot be responsible
    // for the original workload's high-water mark.
    for (index, conn) in connections.iter().enumerate() {
        let cache_rows = conn.query("PRAGMA fsqlite.cache_stats", &[]);
        if cache_rows.is_empty() {
            eprintln!("CACHE connection={} unavailable=true", index + 1);
        }
        for row in cache_rows {
            if let [Cell::Text(name), Cell::Integer(value)] = row.as_slice()
                && [
                    "cached_pages",
                    "capacity_pages",
                    "misses",
                    "total_accesses",
                    "admit_count",
                    "eviction_count",
                ]
                .contains(&name.as_str())
            {
                eprintln!("CACHE connection={} {name}={value}", index + 1);
            }
        }
    }
    while let Some(conn) = connections.pop() {
        conn.close();
    }
    snapshot("closed", detailed);
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert!(
        (5..=6).contains(&args.len()),
        "usage: memory-probe ABS_FIXTURE CONNECTIONS adapter|async|raw mailbox|metrics|open|select [detailed]"
    );
    let path = validate_fixture(Path::new(&args[1]));
    let count: usize = args[2].parse().unwrap();
    assert!((1..=5).contains(&count));
    let backend = match args[3].as_str() {
        "adapter" => Backend::Adapter,
        "async" => Backend::Async,
        "raw" => Backend::Raw,
        _ => panic!("unknown backend"),
    };
    let workload = args[4].clone();
    let detailed = args.get(5).is_some_and(|arg| arg == "detailed");
    // Raw engine futures intentionally get a large stack, just as the
    // AsyncConnection bridge does. Its baseline is taken inside that thread.
    std::thread::Builder::new()
        .name("gh372-probe".into())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || run(path, count, backend, workload, detailed))
        .unwrap()
        .join()
        .unwrap();
}
