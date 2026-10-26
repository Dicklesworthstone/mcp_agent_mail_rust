//! Opt-in, content-free observations of the synchronous archive-save pipeline.
//!
//! The owned worker only samples numeric state: mailbox work and its locks stay
//! on the caller. Like the CLI's other blocking workers, it uses the shared
//! worker-stack policy and is stopped and joined, including on error/unwind.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArchivePhase {
    Prepare,
    SnapshotSource,
    Snapshot,
    Filter,
    Scrub,
    Finalize,
    Metadata,
    Enumerate,
    Zip,
    Publish,
}

impl ArchivePhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::SnapshotSource => "snapshot_source",
            Self::Snapshot => "snapshot",
            Self::Filter => "filter",
            Self::Scrub => "scrub",
            Self::Finalize => "finalize",
            Self::Metadata => "archive_metadata",
            Self::Enumerate => "storage_enumeration",
            Self::Zip => "zip_writing",
            Self::Publish => "publication",
        }
    }
}

impl From<mcp_agent_mail_share::SnapshotPhase> for ArchivePhase {
    fn from(phase: mcp_agent_mail_share::SnapshotPhase) -> Self {
        match phase {
            mcp_agent_mail_share::SnapshotPhase::Snapshot => Self::Snapshot,
            mcp_agent_mail_share::SnapshotPhase::Filter => Self::Filter,
            mcp_agent_mail_share::SnapshotPhase::Scrub => Self::Scrub,
            mcp_agent_mail_share::SnapshotPhase::Finalize => Self::Finalize,
        }
    }
}

#[derive(Default)]
struct Counters {
    storage_files: u64,
    storage_bytes: u64,
    storage_size_unknown: u64,
    zip_files: u64,
    zip_bytes: u64,
    snapshot_bytes: u64,
    archive_bytes: u64,
}

struct State {
    started: Instant,
    phase: Option<(ArchivePhase, Instant)>,
    counters: Counters,
    heartbeat: bool,
    emit: Box<dyn Fn(&str) + Send + Sync>,
}

impl State {
    fn report(&self, status: &str, now: Instant) {
        let Some((phase, phase_started)) = self.phase else {
            return;
        };
        let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        let counters = &self.counters;
        // Only enums, fixed field names, and saturating numeric values enter a
        // record. Never add paths, labels, SQL, errors, or mailbox metadata here.
        (self.emit)(&format!(
            "archive_save_diagnostic phase={} status={} elapsed_ms={} phase_elapsed_ms={} storage_files={} storage_bytes={} storage_size_unknown={} zip_files={} zip_bytes={} snapshot_bytes={} archive_bytes={} heartbeat={}",
            phase.as_str(),
            status,
            millis(now.saturating_duration_since(self.started)),
            millis(now.saturating_duration_since(phase_started)),
            counters.storage_files,
            counters.storage_bytes,
            counters.storage_size_unknown,
            counters.zip_files,
            counters.zip_bytes,
            counters.snapshot_bytes,
            counters.archive_bytes,
            u8::from(self.heartbeat),
        ));
    }
}

/// Disabled instances allocate nothing, spawn nothing, and do no extra I/O.
pub(crate) struct ArchiveDiagnostics {
    state: Option<Arc<Mutex<State>>>,
    stop: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl ArchiveDiagnostics {
    pub(crate) const fn disabled() -> Self {
        Self {
            state: None,
            stop: None,
            worker: None,
        }
    }

    pub(crate) fn new(enabled: bool) -> Self {
        if !enabled {
            return Self::disabled();
        }
        Self::with_sink(Box::new(emit_record), REPORT_INTERVAL)
    }

    #[cfg(test)]
    pub(crate) fn with_test_sink(emit: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self::with_sink(Box::new(emit), REPORT_INTERVAL)
    }

    fn with_sink(emit: Box<dyn Fn(&str) + Send + Sync>, interval: Duration) -> Self {
        let state = Arc::new(Mutex::new(State {
            started: Instant::now(),
            phase: None,
            counters: Counters::default(),
            heartbeat: false,
            emit,
        }));
        let sampled = Arc::clone(&state);
        let (stop, stopped) = mpsc::channel();
        let worker = mcp_agent_mail_core::worker_thread("am-archive-diagnostics")
            .spawn(move || {
                while matches!(
                    stopped.recv_timeout(interval),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    let state = sampled.lock().unwrap_or_else(|error| error.into_inner());
                    state.report("running", Instant::now());
                }
            })
            .ok();
        // If a diagnostic worker cannot be started, phase boundaries still
        // work. Observability must not change the archive result.
        state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .heartbeat = worker.is_some();
        Self {
            state: Some(state),
            stop: Some(stop),
            worker,
        }
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.state.is_some()
    }

    fn update(&self, update: impl FnOnce(&mut State)) {
        if let Some(state) = &self.state {
            update(&mut state.lock().unwrap_or_else(|error| error.into_inner()));
        }
    }

    pub(crate) fn start(&self, phase: ArchivePhase) {
        self.update(|state| {
            let now = Instant::now();
            state.phase = Some((phase, now));
            state.report("started", now);
        });
    }

    pub(crate) fn finish(&self) {
        self.update(|state| {
            state.report("completed", Instant::now());
            state.phase = None;
        });
    }

    pub(crate) fn discovered(&self, bytes: Option<u64>) {
        self.update(|state| {
            let counters = &mut state.counters;
            counters.storage_files = counters.storage_files.saturating_add(1);
            if let Some(bytes) = bytes {
                counters.storage_bytes = counters.storage_bytes.saturating_add(bytes);
            } else {
                counters.storage_size_unknown = counters.storage_size_unknown.saturating_add(1);
            }
        });
    }

    pub(crate) fn snapshot_bytes(&self, bytes: u64) {
        self.update(|state| state.counters.snapshot_bytes = bytes);
    }

    pub(crate) fn archive_bytes(&self, bytes: u64) {
        self.update(|state| state.counters.archive_bytes = bytes);
    }

    fn wrote_bytes(&self, bytes: usize) {
        self.update(|state| {
            state.counters.zip_bytes = state
                .counters
                .zip_bytes
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        });
    }

    fn wrote_file(&self) {
        self.update(|state| {
            state.counters.zip_files = state.counters.zip_files.saturating_add(1);
        });
    }

    pub(crate) fn write_entry(&self, writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
        if self.enabled() {
            CountedWriter {
                writer,
                diagnostics: self,
            }
            .write_all(bytes)?;
            self.wrote_file();
        } else {
            writer.write_all(bytes)?;
        }
        Ok(())
    }

    pub(crate) fn copy_entry(
        &self,
        reader: &mut impl Read,
        writer: &mut impl Write,
    ) -> io::Result<u64> {
        if !self.enabled() {
            return io::copy(reader, writer);
        }
        let bytes = io::copy(
            reader,
            &mut CountedWriter {
                writer,
                diagnostics: self,
            },
        )?;
        self.wrote_file();
        Ok(bytes)
    }
}

impl Drop for ArchiveDiagnostics {
    fn drop(&mut self) {
        self.update(|state| {
            // An active phase on return/unwind did not reach its success
            // boundary. Do not serialize the error (it may contain secrets).
            state.report("failed", Instant::now());
            state.phase = None;
        });
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn emit_record(line: &str) {
    let line = format!("{line}\n");
    if !ftui_runtime::stdio_capture::try_capture(line.as_bytes()) {
        // ftui_eprintln! falls back to eprint!, which panics on a failed
        // stderr write. Diagnostics are best effort, including from Drop.
        let _ = io::stderr().lock().write_all(line.as_bytes());
    }
}

struct CountedWriter<'a, W> {
    writer: &'a mut W,
    diagnostics: &'a ArchiveDiagnostics,
}

impl<W: Write> Write for CountedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(bytes)?;
        self.diagnostics.wrote_bytes(written);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorded() -> (ArchiveDiagnostics, Arc<Mutex<Vec<String>>>) {
        let records = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&records);
        let diagnostics = ArchiveDiagnostics::with_sink(
            Box::new(move |line| sink.lock().unwrap().push(line.to_owned())),
            Duration::from_secs(3600),
        );
        (diagnostics, records)
    }

    fn number(line: &str, key: &str) -> u64 {
        line.split_ascii_whitespace()
            .filter_map(|field| field.split_once('='))
            .find_map(|(name, value)| (name == key).then(|| value.parse().unwrap()))
            .unwrap()
    }

    #[test]
    fn archive_diagnostics_disabled_preserves_io_without_a_worker() {
        let diagnostics = ArchiveDiagnostics::new(false);
        assert!(diagnostics.state.is_none());
        assert!(diagnostics.worker.is_none());
        diagnostics.start(ArchivePhase::Zip);
        let mut output = Vec::new();
        diagnostics.write_entry(&mut output, b"metadata").unwrap();
        diagnostics
            .copy_entry(&mut &b"payload"[..], &mut output)
            .unwrap();
        diagnostics.finish();
        assert_eq!(output, b"metadatapayload");
    }

    #[test]
    fn archive_diagnostics_counters_saturate_without_per_file_output() {
        let (diagnostics, records) = recorded();
        diagnostics.start(ArchivePhase::Enumerate);
        diagnostics.discovered(Some(u64::MAX));
        for _ in 0..10_000 {
            diagnostics.discovered(Some(1));
        }
        diagnostics.discovered(None);
        diagnostics.update(|state| {
            state.counters.zip_files = u64::MAX;
            state.counters.zip_bytes = u64::MAX;
        });
        diagnostics.wrote_file();
        diagnostics.wrote_bytes(1);
        diagnostics.finish();
        drop(diagnostics);
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 2, "counters must not emit one line per file");
        let completed = &records[1];
        assert!(completed.contains("status=completed"));
        assert_eq!(number(completed, "storage_files"), 10_002);
        assert_eq!(number(completed, "storage_bytes"), u64::MAX);
        assert_eq!(number(completed, "storage_size_unknown"), 1);
        assert_eq!(number(completed, "zip_files"), u64::MAX);
        assert_eq!(number(completed, "zip_bytes"), u64::MAX);
        assert!(
            completed.len() < 512,
            "even maximal counters have bounded records"
        );
    }

    #[test]
    fn archive_diagnostics_reports_during_opaque_work_and_joins_on_drop() {
        let (send, receive) = mpsc::channel();
        let diagnostics = ArchiveDiagnostics::with_sink(
            Box::new(move |line| {
                let _ = send.send(line.to_owned());
            }),
            Duration::from_millis(20),
        );
        diagnostics.start(ArchivePhase::Finalize);
        let started = receive.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(started.contains("phase=finalize status=started"));
        // The caller is blocked with no cooperative progress callbacks. Only
        // the owned reporter can produce this observation.
        let running = receive.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(running.contains("phase=finalize status=running"));
        assert!(number(&running, "phase_elapsed_ms") >= 1);
        diagnostics.finish();
        drop(diagnostics);
        let tail: Vec<_> = receive.iter().collect();
        assert!(tail.last().unwrap().contains("status=completed"));
        assert!(tail.iter().all(|line| !line.contains("status=failed")));
        // recv_timeout disconnect proves the worker and sink were released.
        assert_eq!(receive.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    struct FailAfter {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl Write for FailAfter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let remaining = self.limit.saturating_sub(self.bytes.len());
            if remaining == 0 {
                return Err(io::Error::other("PRIVATE-CREDENTIAL-in-error"));
            }
            let written = bytes.len().min(remaining);
            self.bytes.extend_from_slice(&bytes[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn archive_diagnostics_zip_failure_keeps_partial_bytes_without_completing_file() {
        let (diagnostics, records) = recorded();
        diagnostics.start(ArchivePhase::Zip);
        let mut output = FailAfter {
            bytes: Vec::new(),
            limit: 3,
        };
        let error = diagnostics
            .copy_entry(&mut &b"private payload"[..], &mut output)
            .unwrap_err();
        assert_eq!(error.to_string(), "PRIVATE-CREDENTIAL-in-error");
        drop(diagnostics);
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 2);
        let failed = &records[1];
        assert!(failed.contains("phase=zip_writing status=failed"));
        assert_eq!(number(failed, "zip_bytes"), 3);
        assert_eq!(number(failed, "zip_files"), 0);
        assert!(!failed.contains("PRIVATE"));
        assert!(!failed.contains("private"));
        assert_eq!(output.bytes, b"pri");
    }

    #[test]
    fn archive_diagnostics_success_counts_empty_and_large_entries() {
        let (diagnostics, records) = recorded();
        diagnostics.start(ArchivePhase::Zip);
        let mut output = Vec::new();
        diagnostics.write_entry(&mut output, b"").unwrap();
        let payload = vec![42; 256 * 1024];
        diagnostics
            .copy_entry(&mut payload.as_slice(), &mut output)
            .unwrap();
        diagnostics.finish();
        drop(diagnostics);
        assert_eq!(output, payload);
        let records = records.lock().unwrap();
        assert_eq!(number(&records[1], "zip_files"), 2);
        assert_eq!(number(&records[1], "zip_bytes"), payload.len() as u64);
        assert!(records[1].contains("status=completed"));
    }
}
