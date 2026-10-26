//! Opt-in, content-free observations of the synchronous archive-save pipeline.
//!
//! The owned worker only samples numeric state: mailbox work and its locks stay
//! on the caller. Like the CLI's other blocking workers, it uses the shared
//! worker-stack policy and is stopped and joined, including on error/unwind.

use std::io::{self, Write};
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
    Verify,
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
            Self::Verify => "archive_verification",
            Self::Publish => "publication",
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

struct ProgressSample {
    phase: &'static str,
    files: u64,
    bytes: u64,
}

struct State {
    started: Instant,
    phase: Option<(ArchivePhase, Instant)>,
    counters: Counters,
    progress_sample: Option<ProgressSample>,
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
            progress_sample: None,
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

    /// Feed the cooperative, fine-grained save events into the independent
    /// heartbeat. Adjacent fine phases may share one diagnostic phase, and
    /// their local counters must not reset the cumulative diagnostic totals.
    pub(crate) fn observe_progress(&self, event: &super::ArchiveProgressEvent) {
        let phase = match event.phase {
            "prepare" | "mailbox_lock" => ArchivePhase::Prepare,
            "source_reconciliation" => ArchivePhase::SnapshotSource,
            "snapshot_copy" => ArchivePhase::Snapshot,
            "snapshot_scope" => ArchivePhase::Filter,
            "snapshot_scrub" => ArchivePhase::Scrub,
            "snapshot_compact" | "snapshot_viewer" => ArchivePhase::Finalize,
            "metadata" => ArchivePhase::Metadata,
            "storage_inventory" => ArchivePhase::Enumerate,
            "compress_snapshot" | "compress_storage" | "finalize_zip" => ArchivePhase::Zip,
            "verify_zip" => ArchivePhase::Verify,
            "publish" => ArchivePhase::Publish,
            _ => return,
        };
        self.update(|state| {
            let now = Instant::now();
            let changed_phase = state.phase.map(|(active, _)| active) != Some(phase);
            if changed_phase {
                state.report("completed", now);
                state.phase = Some((phase, now));
            }

            let (files, bytes) = state
                .progress_sample
                .as_ref()
                .filter(|sample| sample.phase == event.phase)
                .map_or((event.files, event.bytes), |sample| {
                    (
                        event.files.saturating_sub(sample.files),
                        event.bytes.saturating_sub(sample.bytes),
                    )
                });
            let counters = &mut state.counters;
            match event.phase {
                "storage_inventory" => {
                    counters.storage_files = counters.storage_files.saturating_add(files);
                    counters.storage_bytes = counters.storage_bytes.saturating_add(bytes);
                }
                "compress_snapshot" | "compress_storage" => {
                    counters.zip_files = counters.zip_files.saturating_add(files);
                    counters.zip_bytes = counters.zip_bytes.saturating_add(bytes);
                }
                // Verification reads every member again. It must not count
                // those bytes or files as additional ZIP payload.
                _ => {}
            }
            counters.snapshot_bytes = event.snapshot_bytes;
            counters.archive_bytes = event.archive_bytes;
            state.progress_sample = Some(ProgressSample {
                phase: event.phase,
                files: event.files,
                bytes: event.bytes,
            });

            if changed_phase {
                state.report("started", now);
            }
            if phase == ArchivePhase::Publish && event.state == "completed" {
                state.report("completed", now);
                state.phase = None;
            }
            // A completed fine phase can be followed by another fine phase
            // with the same coarse name. Only a coarse transition or the
            // successful publication ends it. Failure/cancellation leaves it
            // active so Drop reports one terminal failure and joins the worker.
        });
    }

    pub(crate) fn archive_bytes(&self, bytes: u64) {
        self.update(|state| state.counters.archive_bytes = bytes);
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

    fn progress(
        phase: &'static str,
        state: &'static str,
        files: u64,
        bytes: u64,
    ) -> crate::ArchiveProgressEvent {
        let mut observer = |_: &crate::ArchiveProgressEvent| Ok(());
        let mut event = crate::ArchiveSaveProgress::new(
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            &mut observer,
        )
        .event;
        event.phase = phase;
        event.state = state;
        event.files = files;
        event.bytes = bytes;
        event.snapshot_bytes = 7;
        event.archive_bytes = 24;
        event
    }

    fn with_zip_progress<T>(
        diagnostics: &ArchiveDiagnostics,
        run: impl FnOnce(&mut crate::ArchiveSaveProgress<'_>) -> crate::CliResult<T>,
    ) -> crate::CliResult<T> {
        let mut observer = |event: &crate::ArchiveProgressEvent| {
            diagnostics.observe_progress(event);
            Ok(())
        };
        let mut progress = crate::ArchiveSaveProgress::new(
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            &mut observer,
        );
        progress.event.phase = "compress_storage";
        progress.emit("started")?;
        let result = run(&mut progress);
        progress.emit(if result.is_ok() {
            "completed"
        } else {
            "failed"
        })?;
        result
    }

    #[test]
    fn archive_diagnostics_progress_bridge_coalesces_phases_and_counts_payload_once() {
        let (diagnostics, records) = recorded();
        for (phase, state, files, bytes) in [
            ("prepare", "started", 0, 0),
            ("prepare", "completed", 0, 0),
            ("mailbox_lock", "started", 0, 0),
            ("mailbox_lock", "completed", 0, 0),
            ("source_reconciliation", "started", 0, 0),
            ("source_reconciliation", "completed", 0, 0),
            ("snapshot_copy", "started", 0, 0),
            ("snapshot_copy", "completed", 0, 0),
            ("snapshot_scope", "started", 0, 0),
            ("snapshot_scope", "completed", 0, 0),
            ("snapshot_scrub", "started", 0, 0),
            ("snapshot_scrub", "completed", 0, 0),
            ("snapshot_compact", "started", 0, 0),
            ("snapshot_compact", "completed", 0, 0),
            ("snapshot_viewer", "started", 0, 0),
            ("snapshot_viewer", "completed", 0, 0),
            ("metadata", "started", 0, 0),
            ("metadata", "completed", 0, 0),
            ("storage_inventory", "started", 0, 0),
            ("storage_inventory", "progress", 1, 3),
            ("storage_inventory", "completed", 2, 10),
            ("storage_inventory", "completed", 2, 10),
            ("compress_snapshot", "started", 0, 0),
            ("compress_snapshot", "progress", 0, 3),
            ("compress_snapshot", "completed", 2, 20),
            ("compress_storage", "started", 0, 0),
            ("compress_storage", "progress", 1, 4),
            ("compress_storage", "progress", 1, 4),
            ("compress_storage", "completed", 2, 10),
            ("finalize_zip", "started", 0, 0),
            ("finalize_zip", "completed", 0, 0),
            ("verify_zip", "started", 0, 0),
            ("verify_zip", "progress", 3, 25),
            ("verify_zip", "completed", 4, 30),
            ("publish", "started", 0, 0),
            ("publish", "completed", 0, 0),
        ] {
            diagnostics.observe_progress(&progress(phase, state, files, bytes));
        }
        drop(diagnostics);
        let records = records.lock().unwrap();
        let phases = [
            "prepare",
            "snapshot_source",
            "snapshot",
            "filter",
            "scrub",
            "finalize",
            "archive_metadata",
            "storage_enumeration",
            "zip_writing",
            "archive_verification",
            "publication",
        ];
        assert_eq!(records.len(), phases.len() * 2);
        for (phase, pair) in phases.iter().zip(records.chunks_exact(2)) {
            assert!(pair[0].contains(&format!("phase={phase} status=started")));
            assert!(pair[1].contains(&format!("phase={phase} status=completed")));
        }
        let completed = records.last().unwrap();
        assert_eq!(number(completed, "storage_files"), 2);
        assert_eq!(number(completed, "storage_bytes"), 10);
        assert_eq!(number(completed, "storage_size_unknown"), 0);
        assert_eq!(number(completed, "zip_files"), 4);
        assert_eq!(number(completed, "zip_bytes"), 30);
        assert_eq!(number(completed, "snapshot_bytes"), 7);
        assert_eq!(number(completed, "archive_bytes"), 24);
        assert!(
            records
                .iter()
                .all(|record| !record.contains("status=failed"))
        );
    }

    #[test]
    fn archive_diagnostics_progress_bridge_keeps_failure_active_until_drop() {
        for terminal in ["failed", "cancelled"] {
            let (diagnostics, records) = recorded();
            for (phase, state, files, bytes) in [
                ("compress_snapshot", "started", 0, 0),
                ("compress_snapshot", "completed", 2, 20),
                ("compress_storage", "started", 0, 0),
                ("compress_storage", "progress", 1, 4),
                ("compress_storage", terminal, 1, 7),
            ] {
                diagnostics.observe_progress(&progress(phase, state, files, bytes));
            }
            assert_eq!(records.lock().unwrap().len(), 1);
            drop(diagnostics);
            let records = records.lock().unwrap();
            assert_eq!(records.len(), 2);
            assert!(records[1].contains("phase=zip_writing status=failed"));
            assert_eq!(number(&records[1], "zip_files"), 3);
            assert_eq!(number(&records[1], "zip_bytes"), 27);
            assert!(
                records
                    .iter()
                    .all(|record| !record.contains("status=completed"))
            );
        }
    }

    #[test]
    fn archive_diagnostics_disabled_preserves_io_without_a_worker() {
        let diagnostics = ArchiveDiagnostics::new(false);
        assert!(diagnostics.state.is_none());
        assert!(diagnostics.worker.is_none());
        let mut output = Vec::new();
        with_zip_progress(&diagnostics, |progress| {
            crate::archive_write_all(&mut output, b"metadata", false, progress)?;
            progress.advance(1, 0, false)?;
            crate::archive_copy_exact(&mut &b"payload"[..], &mut output, 7, false, progress)?;
            progress.advance(1, 0, false)
        })
        .unwrap();
        diagnostics.finish();
        assert_eq!(output, b"metadatapayload");
        assert!(diagnostics.state.is_none());
        assert!(diagnostics.worker.is_none());
    }

    #[test]
    fn archive_diagnostics_counters_saturate_without_per_file_output() {
        let (diagnostics, records) = recorded();
        diagnostics.observe_progress(&progress("storage_inventory", "started", 0, 0));
        for files in 1..=10_001 {
            diagnostics.observe_progress(&progress(
                "storage_inventory",
                "progress",
                files,
                u64::MAX,
            ));
        }
        // ZIP counters accumulate across separate fine phases. Advancing the
        // next phase after a maximum-sized snapshot must saturate, not wrap.
        for (phase, state, files, bytes) in [
            ("storage_inventory", "completed", 10_001, u64::MAX),
            ("compress_snapshot", "started", 0, 0),
            ("compress_snapshot", "completed", u64::MAX, u64::MAX),
            ("compress_storage", "started", 0, 0),
            ("compress_storage", "completed", 1, 1),
        ] {
            diagnostics.observe_progress(&progress(phase, state, files, bytes));
        }
        diagnostics.finish();
        drop(diagnostics);
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 4, "counters must not emit one line per file");
        let completed = records.last().unwrap();
        assert!(completed.contains("status=completed"));
        assert_eq!(number(completed, "storage_files"), 10_001);
        assert_eq!(number(completed, "storage_bytes"), u64::MAX);
        // Inventory requires metadata, so no synthetic unknown-size branch
        // contributes to the retained diagnostic-schema field.
        assert_eq!(number(completed, "storage_size_unknown"), 0);
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
        let mut output = FailAfter {
            bytes: Vec::new(),
            limit: 3,
        };
        let error = with_zip_progress(&diagnostics, |progress| {
            crate::archive_copy_exact(
                &mut &b"private payload"[..],
                &mut output,
                15,
                false,
                progress,
            )?;
            progress.advance(1, 0, false)
        })
        .unwrap_err();
        let crate::CliError::Io(error) = error else {
            panic!("expected the injected writer I/O failure");
        };
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
        let mut output = Vec::new();
        let payload = vec![42; 256 * 1024];
        with_zip_progress(&diagnostics, |progress| {
            crate::archive_write_all(&mut output, b"", false, progress)?;
            progress.advance(1, 0, false)?;
            crate::archive_copy_exact(
                &mut payload.as_slice(),
                &mut output,
                payload.len() as u64,
                false,
                progress,
            )?;
            progress.advance(1, 0, false)
        })
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
