//! GH373: retention admission against real Linux readers and process permissions.
//!
//! All mutations are confined to private fixtures. The public retention guard
//! verifies real SQLite images; no procfs, unlink, or recovery-lock result is
//! substituted. Opaque-process tests run alone under nextest because their
//! nondumpable children deliberately make other same-user reader scans uncertain.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mcp_agent_mail_db::CanonicalDbConn;
use mcp_agent_mail_db::recovery_retention::CompletedReclaimDelete;
use mcp_agent_mail_db::snapshot::{
    SnapshotRetentionGuard, record_snapshot_metadata, snapshot_bak_path, snapshot_meta_path,
    verified_snapshot_source_path,
};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_ROOT: &str = "AM_RETENTION_LINUX_WORKER_ROOT";
const WORKER_ROLE: &str = "AM_RETENTION_LINUX_WORKER_ROLE";
const INCOMPLETE: &[u8] = b"interrupted backup export; retain for investigation";
static READER_FIXTURE: Mutex<()> = Mutex::new(());

fn reader_fixture_guard() -> MutexGuard<'static, ()> {
    READER_FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct TestProcess(Option<Child>);

impl TestProcess {
    fn spawn(command: &mut Command) -> Self {
        Self(Some(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn owned retention fixture process"),
        ))
    }

    fn wait_for(&mut self, ready: &Path) {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while !ready.exists() {
            if self.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                let output = self.output();
                panic!(
                    "fixture exited before {}: stdout={} stderr={}",
                    ready.display(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
            assert!(Instant::now() < deadline, "fixture readiness timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn output(&mut self) -> Output {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "fixture termination timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        self.0.take().unwrap().wait_with_output().unwrap()
    }

    fn release(mut self) -> Output {
        writeln!(self.0.as_mut().unwrap().stdin.take().unwrap(), "release").unwrap();
        self.output()
    }

    fn kill(mut self) -> Output {
        self.0.as_mut().unwrap().kill().unwrap();
        self.output()
    }
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "fixture failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn spawn_test(test: &str, configure: impl FnOnce(&mut Command)) -> TestProcess {
    // A root-built test executable can have private ancestors. Retain our
    // actual ELF through procfs before changing credentials; this never changes
    // directory permissions or permits selection of another executable.
    let executable = File::open("/proc/self/exe").unwrap();
    let mut command = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()));
    command.args([
        "--exact",
        &mcp_agent_mail_test_helpers::libtest_path!(test),
        "--test-threads=1",
        "--nocapture",
    ]);
    configure(&mut command);
    TestProcess::spawn(&mut command)
}

fn rerun_unprivileged_if_root(test: &str) -> bool {
    if fs::metadata("/proc/self").unwrap().uid() != 0 {
        return false;
    }
    let mut process = spawn_test(test, |command| {
        command.uid(65_534).gid(65_534).env("TMPDIR", "/tmp");
    });
    let output = process.output();
    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
        "the unprivileged child must run the exact test"
    );
    true
}

struct Mailbox {
    root: PathBuf,
    primary: PathBuf,
    snapshot: PathBuf,
    bytes: Vec<u8>,
    metadata: Vec<u8>,
}

impl Mailbox {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("am-retention-linux-")
            .tempdir()
            .unwrap()
            .keep()
            .canonicalize()
            .unwrap();
        let primary = root.join("storage.sqlite3");
        {
            let connection = CanonicalDbConn::open_file(primary.to_str().unwrap()).unwrap();
            connection
                .execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                .unwrap();
            connection
                .execute_raw(
                    "INSERT INTO projects (id, slug, human_key, created_at)
                         VALUES (1, 'retention', '/retention-linux-fixture', 1);
                     INSERT INTO agents
                         (id, project_id, name, program, model, task_description,
                          inception_ts, last_active_ts, attachments_policy, contact_policy,
                          reaper_exempt, registration_token)
                         VALUES (1, 1, 'BlueLake', 'test', 'test', '', 1, 1,
                                 'auto', 'auto', 0, NULL);
                     INSERT INTO messages
                         (id, project_id, sender_id, subject, body_md, importance,
                          ack_required, created_ts, recipients_json, attachments)
                         VALUES (1, 1, 1, 'retained', 'recovery bytes', 'normal',
                                 0, 1, '{}', '[]');",
                )
                .unwrap();
        }
        let snapshot = snapshot_bak_path(&primary);
        fs::copy(&primary, &snapshot).unwrap();
        let created = chrono::DateTime::parse_from_rfc3339("2026-11-02T00:00:00Z")
            .unwrap()
            .timestamp_micros();
        record_snapshot_metadata(&primary, created).unwrap();
        let bytes = fs::read(&primary).unwrap();
        let metadata = fs::read(snapshot_meta_path(&primary)).unwrap();
        Self {
            root,
            primary,
            snapshot,
            bytes,
            metadata,
        }
    }

    fn generation(&self, day: u8) -> PathBuf {
        let path = self
            .root
            .join(format!("storage.sqlite3.bak.200001{day:02}_000000"));
        fs::copy(&self.primary, &path).unwrap();
        path
    }

    fn assert_recovery_unchanged(&self) {
        assert_eq!(fs::read(&self.primary).unwrap(), self.bytes);
        assert_eq!(fs::read(&self.snapshot).unwrap(), self.bytes);
        assert_eq!(
            fs::read(snapshot_meta_path(&self.primary)).unwrap(),
            self.metadata
        );
        assert_eq!(
            verified_snapshot_source_path(&self.primary),
            Some(self.snapshot.clone())
        );
    }
}

const READER: &str = r"
import ctypes
import json
import mmap
import os
import sys
import threading
import time
import traceback

mode, source, ready, opaque, name, remove_alias = sys.argv[1:]
libc = ctypes.CDLL(None, use_errno=True)
libc.prctl.argtypes = [ctypes.c_int] + [ctypes.c_ulong] * 4
dead_leader = mode.startswith('dead-')
if dead_leader:
    mode = mode.removeprefix('dead-')
acquired = threading.Event()

def hold_reader():
    handle = None
    mapping = None
    identity = None
    if mode != 'idle':
        flags = os.O_PATH if mode == 'opath' else os.O_RDONLY
        handle = os.open(source, flags | os.O_CLOEXEC)
        identity = os.fstat(handle)
        if mode == 'mmap':
            mapping = mmap.mmap(handle, 0, access=mmap.ACCESS_READ)
            # Python versions before 3.13 retain an extra descriptor. Close
            # every descriptor for a genuinely descriptorless mapping.
            for entry in os.listdir('/proc/thread-self/fd'):
                descriptor = int(entry)
                try:
                    observed = os.fstat(descriptor)
                except OSError:
                    continue
                if (observed.st_dev, observed.st_ino) == (identity.st_dev, identity.st_ino):
                    os.close(descriptor)
            handle = None
        if remove_alias == 'true':
            os.unlink(source)
    comm = ctypes.create_string_buffer(name.encode())
    assert libc.prctl(15, ctypes.addressof(comm), 0, 0, 0) == 0
    if opaque == 'true':
        assert libc.prctl(4, 0, 0, 0, 0) == 0
    acquired.set()
    if dead_leader:
        deadline = time.monotonic() + 10
        while True:
            with open('/proc/self/status', encoding='utf-8') as status:
                state = next(line for line in status if line.startswith('State:'))
            if state.split()[1] == 'Z':
                break
            assert time.monotonic() < deadline, 'leader did not exit'
            time.sleep(0.005)
    # The proc mount can use an outer PID namespace. Report its actual paths
    # instead of assuming subprocess.pid names the directory the scan sees.
    record = {
        'proc_pid': int(os.readlink('/proc/self')),
        'task_path': os.readlink('/proc/thread-self'),
    }
    with open(ready + '.tmp', 'w', encoding='utf-8') as output:
        json.dump(record, output)
    os.replace(ready + '.tmp', ready)
    assert sys.stdin.readline().strip() == 'release'
    if mapping is not None:
        assert mapping[:16] == b'SQLite format 3\x00'
        mapping.close()
    elif handle is not None:
        assert os.fstat(handle).st_ino == identity.st_ino
        os.close(handle)

def worker_main():
    try:
        hold_reader()
    except BaseException:
        # An unhandled Python thread exception alone does not make the process
        # fail once its main pthread has exited. Preserve failed reader checks.
        traceback.print_exc()
        os._exit(1)

if dead_leader:
    threading.Thread(target=worker_main).start()
    assert acquired.wait(10), 'worker did not acquire its reader'
    # CDLL releases the GIL around this call. The main Linux thread exits while
    # the Python worker remains alive with the real descriptor or mapping.
    libc.pthread_exit.argtypes = [ctypes.c_void_p]
    libc.pthread_exit.restype = None
    libc.pthread_exit(None)
else:
    hold_reader()
";

fn reader(
    root: &Path,
    source: &Path,
    mode: &str,
    opaque: bool,
    name: &str,
    remove_alias: bool,
) -> (TestProcess, PathBuf) {
    let ready = root.join("reader-ready.json");
    let mut command = Command::new("python3");
    command
        .args(["-u", "-c", READER, mode])
        .arg(source)
        .arg(&ready)
        .arg(if opaque { "true" } else { "false" })
        .arg(name)
        .arg(if remove_alias { "true" } else { "false" });
    let mut process = TestProcess::spawn(&mut command);
    process.wait_for(&ready);
    let record: serde_json::Value = serde_json::from_slice(&fs::read(ready).unwrap()).unwrap();
    let proc = PathBuf::from(format!("/proc/{}", record["proc_pid"].as_u64().unwrap()));
    (process, proc)
}

#[test]
fn external_descriptors_and_mappings_preserve_backups_until_reader_exit() {
    let _serial = reader_fixture_guard();
    if rerun_unprivileged_if_root(
        "external_descriptors_and_mappings_preserve_backups_until_reader_exit",
    ) {
        return;
    }
    for mode in ["fd", "opath", "mmap"] {
        let mailbox = Mailbox::new();
        let generation = mailbox.generation(1);
        let alias = mailbox.root.join("removed-reader-alias");
        let reader_path = if mode == "mmap" {
            fs::hard_link(&generation, &alias).unwrap();
            &alias
        } else {
            &generation
        };
        let (process, proc) = reader(
            &mailbox.root,
            reader_path,
            mode,
            false,
            "retention-read",
            mode == "mmap",
        );
        let inventoried = fs::metadata(&generation).unwrap();
        if mode == "mmap" {
            assert!(!alias.exists());
            assert_eq!(inventoried.nlink(), 1);
            for descriptor in fs::read_dir(proc.join("fd")).unwrap() {
                let observed = fs::metadata(descriptor.unwrap().path()).unwrap();
                assert_ne!(
                    (observed.dev(), observed.ino()),
                    (inventoried.dev(), inventoried.ino()),
                    "mmap fixture must retain no descriptor for the backup"
                );
            }
        }
        let guard = SnapshotRetentionGuard::acquire(&mailbox.primary).unwrap();
        let error = guard
            .delete_superseded_generation(&mailbox.primary, &generation, &inventoried)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{mode}: {error}");
        assert!(error.to_string().contains(if mode == "mmap" {
            "memory mapping"
        } else {
            "file reader"
        }));
        assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
        assert_eq!(fs::read(&generation).unwrap(), mailbox.bytes);
        assert_success(&process.release());
        let removed = guard
            .delete_superseded_generation(&mailbox.primary, &generation, &inventoried)
            .unwrap();
        assert_eq!(removed.logical_bytes, inventoried.len());
        assert!(!generation.exists());
        drop(guard);
        mailbox.assert_recovery_unchanged();
    }
}

#[test]
fn nondumpable_process_names_never_authorize_deletion() {
    let _serial = reader_fixture_guard();
    if rerun_unprivileged_if_root("nondumpable_process_names_never_authorize_deletion") {
        return;
    }
    for name in [
        "retention-read",
        "(sd-pam)",
        "sshd-session",
        "sshd",
        "ssh-agent",
        "gpg-agent",
    ] {
        for mode in ["idle", "fd", "opath", "mmap"] {
            let mailbox = Mailbox::new();
            let generation = mailbox.generation(1);
            let (process, proc) = reader(&mailbox.root, &generation, mode, true, name, false);
            // Real nondumpability, not a synthetic PermissionDenied error.
            assert_eq!(
                fs::read_dir(proc.join("fd")).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                fs::read(proc.join("maps")).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            let status = fs::read_to_string(proc.join("status")).unwrap();
            let own_status = fs::read_to_string("/proc/self/status").unwrap();
            assert_eq!(
                status.lines().find(|line| line.starts_with("Uid:")),
                own_status.lines().find(|line| line.starts_with("Uid:"))
            );
            assert_eq!(fs::metadata(proc.join("fd")).unwrap().uid(), 0);
            let inventoried = fs::metadata(&generation).unwrap();
            let guard = SnapshotRetentionGuard::acquire(&mailbox.primary).unwrap();
            let result =
                guard.delete_superseded_generation(&mailbox.primary, &generation, &inventoried);
            // Even an unrelated opaque process is not proof of no reader:
            // the same credentials and mutable comm name admit all four cases.
            assert_success(&process.release());
            let Err(error) = result else {
                panic!("opaque {name}/{mode} authorized deletion");
            };
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("{}/", proc.display()))
            );
            assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
            assert_eq!(fs::read(&generation).unwrap(), mailbox.bytes);
            guard
                .delete_superseded_generation(&mailbox.primary, &generation, &inventoried)
                .unwrap();
            assert!(!generation.exists());
            drop(guard);
            mailbox.assert_recovery_unchanged();
        }
    }
}

#[test]
fn exited_thread_group_leader_does_not_hide_a_live_backup_reader() {
    let _serial = reader_fixture_guard();
    if rerun_unprivileged_if_root("exited_thread_group_leader_does_not_hide_a_live_backup_reader") {
        return;
    }
    for opaque in [false, true] {
        for mode in ["dead-idle", "dead-fd", "dead-opath", "dead-mmap"] {
            let mailbox = Mailbox::new();
            let generation = mailbox.generation(1);
            let (process, proc) =
                reader(&mailbox.root, &generation, mode, opaque, "ssh-agent", false);
            match fs::read_dir(proc.join("fd")) {
                Ok(descriptors) => assert_eq!(descriptors.count(), 0),
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::PermissionDenied),
            }
            assert_eq!(fs::read(proc.join("maps")).unwrap(), Vec::<u8>::new());
            let leader_status = fs::read_to_string(proc.join("status")).unwrap();
            assert!(leader_status.lines().any(|line| {
                line.strip_prefix("State:")
                    .is_some_and(|state| state.trim_start().starts_with('Z'))
            }));
            assert!(
                !leader_status
                    .lines()
                    .any(|line| line.starts_with("VmSize:"))
            );
            assert!(leader_status.lines().any(|line| {
                line.strip_prefix("FDSize:")
                    .is_some_and(|size| size.trim() == "0")
            }));
            let readiness: serde_json::Value =
                serde_json::from_slice(&fs::read(mailbox.root.join("reader-ready.json")).unwrap())
                    .unwrap();
            let task = Path::new("/proc").join(readiness["task_path"].as_str().unwrap());
            assert!(task.starts_with(&proc));
            if opaque {
                assert_eq!(
                    fs::read_dir(task.join("fd")).unwrap_err().kind(),
                    io::ErrorKind::PermissionDenied
                );
            } else {
                assert_ne!(fs::read(task.join("maps")).unwrap(), Vec::<u8>::new());
            }
            let inventoried = fs::metadata(&generation).unwrap();
            let guard = SnapshotRetentionGuard::acquire(&mailbox.primary).unwrap();
            let result =
                guard.delete_superseded_generation(&mailbox.primary, &generation, &inventoried);
            assert_success(&process.release());
            if mode == "dead-idle" && !opaque {
                // A terminal leader's inaccessible descriptor directory is
                // unrelated once its still-live, inspectable workers are clear.
                assert_eq!(result.unwrap().logical_bytes, inventoried.len());
            } else {
                let error = result.expect_err("a dead leader does not release its worker's reader");
                assert_eq!(
                    error.kind(),
                    if opaque {
                        io::ErrorKind::PermissionDenied
                    } else {
                        io::ErrorKind::WouldBlock
                    },
                    "{mode}/{opaque}: {error}"
                );
                assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
                assert_eq!(fs::read(&generation).unwrap(), mailbox.bytes);
                guard
                    .delete_superseded_generation(&mailbox.primary, &generation, &inventoried)
                    .unwrap();
            }
            assert!(!generation.exists());
            drop(guard);
            mailbox.assert_recovery_unchanged();
        }
    }
}

#[test]
fn real_unlink_permission_refusal_preserves_bytes_and_accounting() {
    let _serial = reader_fixture_guard();
    if rerun_unprivileged_if_root("real_unlink_permission_refusal_preserves_bytes_and_accounting") {
        return;
    }
    let mailbox = Mailbox::new();
    let parked = mailbox.root.join("doctor/reclaimable/rotation-permission");
    fs::create_dir_all(&parked).unwrap();
    let generation = parked.join("storage.sqlite3.bak.20000101_000000");
    fs::copy(&mailbox.primary, &generation).unwrap();
    let inventoried = fs::metadata(&generation).unwrap();
    let guard = SnapshotRetentionGuard::acquire(&mailbox.primary).unwrap();
    fs::set_permissions(&parked, fs::Permissions::from_mode(0o500)).unwrap();
    let result =
        guard.delete_superseded_historical_generation(&mailbox.primary, &generation, &inventoried);
    fs::set_permissions(&parked, fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
    assert_eq!(fs::read(&generation).unwrap(), mailbox.bytes);
    let removed = guard
        .delete_superseded_historical_generation(&mailbox.primary, &generation, &inventoried)
        .unwrap();
    assert_eq!(removed.logical_bytes, inventoried.len());
    assert!(!generation.exists());
    drop(guard);
    mailbox.assert_recovery_unchanged();
}

fn cleanup_worker(root: &Path, role: &str) {
    let primary = root.join("storage.sqlite3");
    let first = root.join("storage.sqlite3.bak.20000101_000000");
    let second = root.join("storage.sqlite3.bak.20000102_000000");
    let partial = root.join("storage.sqlite3.bak.20000103_000000");
    let guard = SnapshotRetentionGuard::acquire(&primary).unwrap();
    assert_eq!(
        guard.verified_source_path(),
        Some(snapshot_bak_path(&primary).as_path())
    );
    match role {
        "interrupt" => {
            guard
                .delete_superseded_generation(&primary, &first, &fs::metadata(&first).unwrap())
                .unwrap();
            fs::write(root.join("first-deleted"), b"admission still held").unwrap();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap();
            panic!("interrupted cleaner unexpectedly released: {input}");
        }
        "resume" => {
            assert!(!first.exists(), "completed deletion must not be replayed");
            guard
                .delete_superseded_generation(&primary, &second, &fs::metadata(&second).unwrap())
                .unwrap();
            let error = guard
                .delete_superseded_generation(&primary, &partial, &fs::metadata(&partial).unwrap())
                .unwrap_err();
            assert!(
                error.to_string().contains("failed full verification"),
                "{error}"
            );
            assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
            assert_eq!(fs::read(partial).unwrap(), INCOMPLETE);
        }
        _ => panic!("unknown cleanup fixture role: {role}"),
    }
}

#[test]
fn concurrent_cleanup_and_process_restart_preserve_recovery_evidence() {
    if let Some(root) = std::env::var_os(WORKER_ROOT) {
        cleanup_worker(Path::new(&root), &std::env::var(WORKER_ROLE).unwrap());
        return;
    }
    let _serial = reader_fixture_guard();
    if rerun_unprivileged_if_root(
        "concurrent_cleanup_and_process_restart_preserve_recovery_evidence",
    ) {
        return;
    }
    let mailbox = Mailbox::new();
    let first = mailbox.generation(1);
    let second = mailbox.generation(2);
    let partial = mailbox.root.join("storage.sqlite3.bak.20000103_000000");
    fs::write(&partial, INCOMPLETE).unwrap();
    let test = "concurrent_cleanup_and_process_restart_preserve_recovery_evidence";
    let mut cleaner = spawn_test(test, |command| {
        command
            .env(WORKER_ROOT, &mailbox.root)
            .env(WORKER_ROLE, "interrupt");
    });
    cleaner.wait_for(&mailbox.root.join("first-deleted"));
    assert!(!first.exists());
    assert_eq!(fs::read(&second).unwrap(), mailbox.bytes);
    let error = SnapshotRetentionGuard::acquire(&mailbox.primary)
        .err()
        .expect("a second cleaner must not acquire the live election");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
    assert_eq!(fs::read(&partial).unwrap(), INCOMPLETE);
    let killed = cleaner.kill();
    assert_eq!(killed.status.signal(), Some(9));

    // A distinct process must rediscover durable metadata and acquire the
    // released kernel election after SIGKILL; no in-process cache survives.
    let mut resumed = spawn_test(test, |command| {
        command
            .env(WORKER_ROOT, &mailbox.root)
            .env(WORKER_ROLE, "resume");
    });
    let output = resumed.output();
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    assert!(!first.exists());
    assert!(!second.exists());
    assert_eq!(fs::read(&partial).unwrap(), INCOMPLETE);
    mailbox.assert_recovery_unchanged();
}
