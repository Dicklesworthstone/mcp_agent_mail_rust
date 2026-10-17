//! Incremental, read-only admission of durable closeout journals.
//!
//! Nothing is eligible for replay until the entire pinned EOF has been checked:
//! a terminal marker can follow its intent, or precede a duplicate of it. Each
//! poll consumes bounded bytes/records and revalidates the retained file handle.
//! Oversized snapshots/state and unsupported schemas are errors, never empty
//! queues. This stricter maintenance reader does not rewrite or compact evidence.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Read};
use std::sync::atomic::{AtomicBool, Ordering};

use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::journal_io::{JournalDirectory, JournalFileMode};
use mcp_agent_mail_tools::degraded_intents::{
    self as journal, QueuedAckIntent, QueuedReleaseIntentView,
};
use serde_json::Value;

const BYTES_PER_POLL: usize = 256 * 1024;
const RECORDS_PER_POLL: usize = 256;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;
const MAX_IDENTITIES: usize = 65_536;

type Key = (String, String);

#[derive(Clone, Copy, Debug)]
pub(super) enum Kind {
    Ack,
    Release,
}

impl Kind {
    const fn file_name(self) -> &'static str {
        match self {
            Self::Ack => journal::ACK_INTENT_LOG_FILE,
            Self::Release => journal::RELEASE_INTENT_LOG_FILE,
        }
    }

    const fn intent_kind(self) -> &'static str {
        match self {
            Self::Ack => journal::ACK_INTENT_KIND,
            Self::Release => journal::RELEASE_INTENT_KIND,
        }
    }

    const fn replay_kind(self) -> &'static str {
        match self {
            Self::Ack => journal::ACK_INTENT_REPLAY_KIND,
            Self::Release => journal::RELEASE_INTENT_REPLAY_KIND,
        }
    }
}

#[derive(Debug)]
pub(super) enum Intent {
    Ack(QueuedAckIntent),
    Release(QueuedReleaseIntentView),
}

impl Intent {
    fn key(&self) -> Key {
        match self {
            Self::Ack(intent) => (intent.intent_id.clone(), intent.content_sha256.clone()),
            Self::Release(intent) => (intent.intent_id.clone(), intent.content_sha256.clone()),
        }
    }
}

#[derive(Clone, Copy)]
struct Limits {
    record_bytes: usize,
    snapshot_bytes: u64,
    pending_bytes: usize,
    identities: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            record_bytes: MAX_RECORD_BYTES,
            snapshot_bytes: MAX_SNAPSHOT_BYTES,
            pending_bytes: MAX_PENDING_BYTES,
            identities: MAX_IDENTITIES,
        }
    }
}

/// `None` from poll means admission is still in progress, not an empty queue.
/// Errors discard partial state; a subsequent poll reopens the current source.
/// The caller controls retry/backoff and never holds a DB connection for a scan.
pub(super) struct Scanner {
    kind: Kind,
    snapshot: Option<Snapshot>,
    limits: Limits,
}

impl Scanner {
    pub(super) fn new(kind: Kind) -> Self {
        Self {
            kind,
            snapshot: None,
            limits: Limits::default(),
        }
    }

    pub(super) fn poll(
        &mut self,
        config: &Config,
        shutdown: &AtomicBool,
    ) -> io::Result<Option<Vec<Intent>>> {
        self.poll_bounded(config, shutdown, BYTES_PER_POLL, RECORDS_PER_POLL)
    }

    fn poll_bounded(
        &mut self,
        config: &Config,
        shutdown: &AtomicBool,
        bytes: usize,
        records: usize,
    ) -> io::Result<Option<Vec<Intent>>> {
        let result = self.advance(config, shutdown, bytes, records);
        if result.is_err() || matches!(result.as_ref(), Ok(Some(_))) {
            self.snapshot = None;
        }
        result
    }

    fn advance(
        &mut self,
        config: &Config,
        shutdown: &AtomicBool,
        bytes: usize,
        records: usize,
    ) -> io::Result<Option<Vec<Intent>>> {
        check_shutdown(shutdown)?;
        if self.snapshot.is_none() {
            self.snapshot = Snapshot::open(config, self.kind, self.limits)?;
        }
        let Some(snapshot) = self.snapshot.as_mut() else {
            return Ok(Some(Vec::new()));
        };
        snapshot.validate()?;
        let complete = snapshot.advance(shutdown, bytes, records)?;
        snapshot.validate()?;
        if !complete {
            return Ok(None);
        }
        if snapshot.skipped > 0 {
            tracing::warn!(
                skipped_records = snapshot.skipped,
                "malformed closeout journal records ignored; intact records retained"
            );
        }
        let mut pending: Vec<_> = std::mem::take(&mut snapshot.pending)
            .into_values()
            .collect();
        pending.sort_unstable_by_key(|(order, _, _)| *order);
        Ok(Some(
            pending.into_iter().map(|(_, intent, _)| intent).collect(),
        ))
    }
}

struct Snapshot {
    kind: Kind,
    directory: JournalDirectory,
    reader: io::BufReader<io::Take<std::fs::File>>,
    initial_bytes: u64,
    line: Vec<u8>,
    pending: HashMap<Key, (u64, Intent, usize)>,
    terminal: HashSet<Key>,
    pending_bytes: usize,
    next_order: u64,
    skipped: u64,
    limits: Limits,
}

impl Snapshot {
    fn open(config: &Config, kind: Kind, limits: Limits) -> io::Result<Option<Self>> {
        let directory = match JournalDirectory::open(
            &config.storage_root,
            journal::DEGRADED_INTENTS_DIR,
            false,
        ) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let file = match directory.open_file(kind.file_name(), JournalFileMode::Read) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let initial_bytes = file.metadata()?.len();
        if initial_bytes > limits.snapshot_bytes {
            return Err(invalid(
                "closeout journal snapshot exceeds the background scan byte limit; journal preserved; operator review required",
            ));
        }
        Ok(Some(Self {
            kind,
            directory,
            reader: io::BufReader::with_capacity(8192, file.take(initial_bytes)),
            initial_bytes,
            line: Vec::new(),
            pending: HashMap::new(),
            terminal: HashSet::new(),
            pending_bytes: 0,
            next_order: 0,
            skipped: 0,
            limits,
        }))
    }

    fn validate(&self) -> io::Result<()> {
        let file = self.reader.get_ref().get_ref();
        self.directory.validate_file(self.kind.file_name(), file)?;
        if file.metadata()?.len() < self.initial_bytes {
            return Err(invalid(
                "closeout journal truncated during snapshot admission; partial queue refused",
            ));
        }
        Ok(())
    }

    fn advance(
        &mut self,
        shutdown: &AtomicBool,
        mut bytes: usize,
        mut records: usize,
    ) -> io::Result<bool> {
        while bytes > 0 && records > 0 {
            check_shutdown(shutdown)?;
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                if self.reader.get_ref().limit() != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "closeout journal truncated before its pinned EOF",
                    ));
                }
                if !self.line.is_empty() {
                    self.accept_line()?;
                }
                return Ok(true);
            }
            let available = &available[..available.len().min(bytes)];
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            if consumed > self.limits.record_bytes.saturating_sub(self.line.len()) {
                return Err(invalid(
                    "closeout journal record exceeds its byte limit; partial queue refused",
                ));
            }
            self.line.extend_from_slice(&available[..consumed]);
            self.reader.consume(consumed);
            bytes -= consumed;
            if newline.is_some() {
                self.accept_line()?;
                records -= 1;
            }
        }
        Ok(false)
    }

    // A new terminal key passes `admit_key` before it is inserted: the limit
    // counts the identities already held, so inserting first would refuse one
    // identity early.
    #[allow(clippy::set_contains_or_insert)]
    fn accept_line(&mut self) -> io::Result<()> {
        let bytes = self.line.len();
        let decoded = if self.line.iter().all(u8::is_ascii_whitespace) {
            None
        } else if let Ok(value) = serde_json::from_slice::<Value>(&self.line) {
            Some(decode(self.kind, value)?)
        } else {
            self.skipped = self.skipped.saturating_add(1);
            None
        };
        self.line.clear();
        match decoded {
            Some(Record::Intent(intent)) => {
                let key = intent.key();
                if self.terminal.contains(&key) || self.pending.contains_key(&key) {
                    return Ok(());
                }
                self.admit_key()?;
                if bytes > self.limits.pending_bytes.saturating_sub(self.pending_bytes) {
                    return Err(invalid(
                        "closeout journal pending payload limit exceeded; partial queue refused",
                    ));
                }
                let order = self.next_order;
                self.next_order = order
                    .checked_add(1)
                    .ok_or_else(|| invalid("closeout journal sequence overflow"))?;
                self.pending_bytes += bytes;
                self.pending.insert(key, (order, intent, bytes));
            }
            Some(Record::Terminal(key)) => {
                if let Some((_, _, bytes)) = self.pending.remove(&key) {
                    self.pending_bytes -= bytes;
                }
                if !self.terminal.contains(&key) {
                    self.admit_key()?;
                    self.terminal.insert(key);
                }
            }
            Some(Record::Ignored) | None => {}
        }
        Ok(())
    }

    fn admit_key(&self) -> io::Result<()> {
        if self.pending.len().saturating_add(self.terminal.len()) >= self.limits.identities {
            return Err(invalid(
                "closeout journal identity limit exceeded; partial queue refused",
            ));
        }
        Ok(())
    }
}

enum Record {
    Intent(Intent),
    Terminal(Key),
    Ignored,
}

fn decode(kind: Kind, record: Value) -> io::Result<Record> {
    let record_kind = record.get("kind").and_then(Value::as_str);
    let is_intent = record_kind == Some(kind.intent_kind());
    let is_replay = record_kind == Some(kind.replay_kind());
    if !is_intent && !is_replay {
        return Ok(Record::Ignored);
    }
    // Validate the schema even on a nonterminal marker. Unknown semantics
    // cannot prove either a pending instruction or successful completion.
    let version = record.get("schema_version").and_then(Value::as_u64);
    let keyed = matches!(kind, Kind::Ack)
        && is_intent
        && version == Some(u64::from(journal::KEYED_ACK_INTENT_SCHEMA_VERSION));
    if version != Some(1) && !keyed {
        return Err(invalid(
            "closeout journal has an unsupported schema; replay stopped and evidence preserved",
        ));
    }
    if !keyed && matches!(kind, Kind::Ack) && is_intent && record.get("idempotency").is_some() {
        return Err(invalid(
            "legacy acknowledgement contains an unsupported retry claim; replay refused",
        ));
    }
    // Field order is part of the hash, so hash through the writers' own
    // projections. A local copy once put a release marker's `released` after
    // `error_detail` and ignored every real completion.
    let payload = match (kind, is_intent) {
        (Kind::Ack, true) => journal::ack_intent_hash_payload(&record),
        (Kind::Ack, false) => journal::ack_replay_hash_payload(&record),
        (Kind::Release, true) => journal::release_intent_hash_payload(&record),
        (Kind::Release, false) => journal::release_replay_hash_payload(&record),
    };
    let Some(hash) = record.get("content_sha256").and_then(Value::as_str) else {
        return Ok(Record::Ignored);
    };
    let Some(id) = record.get("intent_id").and_then(Value::as_str) else {
        return Ok(Record::Ignored);
    };
    if hash.len() != 64 || id.len() != 16 || hash != journal::hash_json_value(&payload) {
        return Ok(Record::Ignored);
    }
    if is_replay {
        let Some(target) = record.get("intent_content_sha256").and_then(Value::as_str) else {
            return Ok(Record::Ignored);
        };
        if target.len() == 64
            && target.starts_with(id)
            && record
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(journal::is_terminal_replay_status)
        {
            return Ok(Record::Terminal((id.to_string(), target.to_string())));
        }
        return Ok(Record::Ignored);
    }
    if !hash.starts_with(id) {
        return Ok(Record::Ignored);
    }
    match kind {
        Kind::Ack => {
            let intent: QueuedAckIntent = serde_json::from_value(record).map_err(|_| {
                invalid("verified acknowledgement has an invalid payload; replay refused")
            })?;
            if keyed {
                let claim = intent.idempotency.as_ref().ok_or_else(|| {
                    invalid("keyed acknowledgement lost its retry claim; replay refused")
                })?;
                if claim.key.is_empty()
                    || claim.key.trim() != claim.key
                    || claim.key.chars().count()
                        > mcp_agent_mail_tools::idempotency::MAX_IDEMPOTENCY_KEY_LEN
                    || claim.fingerprint.len() != 64
                    || !claim
                        .fingerprint
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(invalid(
                        "keyed acknowledgement has an invalid retry claim; replay refused",
                    ));
                }
            }
            Ok(Record::Intent(Intent::Ack(intent)))
        }
        Kind::Release => serde_json::from_value(record)
            .map(|intent| Record::Intent(Intent::Release(intent)))
            .map_err(|_| invalid("verified release has an invalid payload; replay refused")),
    }
}

fn check_shutdown(shutdown: &AtomicBool) -> io::Result<()> {
    if shutdown.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "closeout journal admission cancelled",
        ))
    } else {
        Ok(())
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn fixture() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: dir.path().to_path_buf(),
            ..Config::default()
        };
        (dir, config)
    }

    fn seal(mut record: Value, intent: bool) -> Value {
        record.as_object_mut().unwrap().remove("content_sha256");
        if intent {
            record.as_object_mut().unwrap().remove("intent_id");
        }
        let hash = journal::hash_json_value(&record);
        if intent {
            record["intent_id"] = json!(&hash[..16]);
        }
        record["content_sha256"] = json!(hash);
        record
    }

    // Fixtures use the writers' field order: the hash covers it.
    fn intent(kind: Kind, sequence: i64) -> Value {
        let record = match kind {
            Kind::Ack => json!({
                "schema_version": 1, "kind": kind.intent_kind(), "created_ts": sequence,
                "project_key": "/replay", "agent_name": "BlueLake", "message_id": sequence,
                "failure": {"stage": "test", "error_detail": "busy"},
            }),
            Kind::Release => json!({
                "schema_version": 1, "kind": kind.intent_kind(), "created_ts": sequence,
                "project_key": "/replay", "agent_name": "BlueLake", "paths": null,
                "file_reservation_ids": [sequence],
                "failure": {"stage": "test", "error_detail": "busy"},
            }),
        };
        seal(record, true)
    }

    fn marker(kind: Kind, intent: &Value, status: &str) -> Value {
        let mut record = json!({
            "schema_version": 1, "kind": kind.replay_kind(),
            "intent_id": intent["intent_id"], "intent_content_sha256": intent["content_sha256"],
            "replayed_ts": 100, "status": status,
        });
        if matches!(kind, Kind::Release) {
            record["released"] = json!(0);
        }
        record["error_detail"] = Value::Null;
        seal(record, false)
    }

    fn append(config: &Config, kind: Kind, record: &Value) {
        let lock = format!(".{}.lock", kind.file_name());
        journal::append_jsonl(config, kind.file_name(), &lock, record).unwrap();
    }

    fn finish(scanner: &mut Scanner, config: &Config) -> io::Result<Vec<Intent>> {
        for _ in 0..100_000 {
            if let Some(intents) = scanner.poll_bounded(config, &AtomicBool::new(false), 19, 1)? {
                return Ok(intents);
            }
        }
        panic!("incremental scan did not terminate");
    }

    fn assert_same_as_existing_reader(kind: Kind, config: &Config, intents: Vec<Intent>) {
        match kind {
            Kind::Ack => {
                let observed: Vec<_> = intents
                    .into_iter()
                    .map(|intent| match intent {
                        Intent::Ack(intent) => intent,
                        Intent::Release(_) => panic!("wrong lane"),
                    })
                    .collect();
                assert_eq!(observed, journal::read_queued_ack_intents(config).unwrap());
            }
            Kind::Release => {
                let observed: Vec<_> = intents
                    .into_iter()
                    .map(|intent| match intent {
                        Intent::Release(intent) => intent,
                        Intent::Ack(_) => panic!("wrong lane"),
                    })
                    .collect();
                assert_eq!(
                    observed,
                    journal::read_queued_release_intents(config).unwrap()
                );
            }
        }
    }

    #[test]
    fn completions_from_the_production_writers_are_terminal() {
        // The hash covers field order. A scanner hashing a release marker's
        // fields in its own order ignored every real completion: each
        // replayed release stayed pending, was replayed again on every pass,
        // and grew the journal toward the snapshot limit.
        let (_dir, config) = fixture();
        let done =
            journal::append_ack_intent(&config, "/replay", "BlueLake", 1, "test", "busy", None)
                .unwrap();
        journal::append_ack_replay_record(
            &config,
            &done.intent_id,
            &done.content_sha256,
            journal::REPLAY_STATUS_REPLAYED,
            None,
        );
        let open =
            journal::append_ack_intent(&config, "/replay", "BlueLake", 2, "test", "busy", None)
                .unwrap();
        let pending = finish(&mut Scanner::new(Kind::Ack), &config).unwrap();
        let ids: Vec<_> = pending.iter().map(|intent| intent.key().0).collect();
        assert_eq!(ids, [open.intent_id]);
        assert_same_as_existing_reader(Kind::Ack, &config, pending);

        append(&config, Kind::Release, &intent(Kind::Release, 1));
        append(&config, Kind::Release, &intent(Kind::Release, 2));
        let queued = journal::read_queued_release_intents(&config).unwrap();
        assert_eq!(queued.len(), 2);
        super::super::releases::append_completion(&config, &queued[0], 1).unwrap();
        let pending = finish(&mut Scanner::new(Kind::Release), &config).unwrap();
        let ids: Vec<_> = pending.iter().map(|intent| intent.key().0).collect();
        assert_eq!(ids, [queued[1].intent_id.clone()]);
        assert_same_as_existing_reader(Kind::Release, &config, pending);
    }

    #[test]
    fn incremental_snapshots_match_both_existing_readers_and_terminal_ordering() {
        for kind in [Kind::Ack, Kind::Release] {
            let (_dir, config) = fixture();
            let first = intent(kind, 1);
            let second = intent(kind, 2);
            let third = intent(kind, 3);
            append(
                &config,
                kind,
                &marker(kind, &third, journal::REPLAY_STATUS_ABANDONED),
            );
            append(&config, kind, &first);
            append(&config, kind, &second);
            append(
                &config,
                kind,
                &marker(kind, &first, journal::REPLAY_STATUS_REPLAYED),
            );
            append(&config, kind, &first);
            append(&config, kind, &third);
            append(
                &config,
                kind,
                &marker(kind, &second, journal::REPLAY_STATUS_FAILED),
            );
            let mut scanner = Scanner::new(kind);
            let pending = finish(&mut scanner, &config).unwrap();
            assert_eq!(pending.len(), 1);
            assert_same_as_existing_reader(kind, &config, pending);
            assert!(
                scanner.snapshot.is_none(),
                "completed scan releases its file handles"
            );
        }
    }

    #[test]
    fn no_prefix_is_admitted_before_a_late_terminal_marker() {
        let (_dir, config) = fixture();
        let first = intent(Kind::Ack, 1);
        append(&config, Kind::Ack, &first);
        append(
            &config,
            Kind::Ack,
            &marker(Kind::Ack, &first, journal::REPLAY_STATUS_REPLAYED),
        );
        let mut scanner = Scanner::new(Kind::Ack);
        let stop = AtomicBool::new(false);
        assert!(
            scanner
                .poll_bounded(&config, &stop, 1, 1)
                .unwrap()
                .is_none()
        );
        assert!(scanner.snapshot.is_some());
        assert!(finish(&mut scanner, &config).unwrap().is_empty());
    }

    #[test]
    fn unsupported_intents_and_replay_schemas_refuse_the_entire_snapshot() {
        for kind in [Kind::Ack, Kind::Release] {
            for terminal in [false, true] {
                let (_dir, config) = fixture();
                let first = intent(kind, 1);
                append(&config, kind, &first);
                let mut future = if terminal {
                    marker(kind, &first, journal::REPLAY_STATUS_REPLAYED)
                } else {
                    intent(kind, 2)
                };
                future["schema_version"] = json!(99);
                future = seal(future, !terminal);
                append(&config, kind, &future);
                let path = journal::log_path(&config, kind.file_name());
                let before = std::fs::read(&path).unwrap();
                let mut scanner = Scanner::new(kind);
                let error = finish(&mut scanner, &config).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert!(scanner.snapshot.is_none());
                assert_eq!(std::fs::read(path).unwrap(), before);
            }
        }
    }

    #[test]
    fn keyed_acknowledgements_preserve_claims_and_refuse_downgrades() {
        let (_dir, config) = fixture();
        let claim = journal::AckIntentIdempotency {
            key: "private-key".into(),
            fingerprint: "a".repeat(64),
        };
        journal::append_ack_intent(
            &config,
            "/replay",
            "BlueLake",
            42,
            "test",
            "busy",
            Some(&claim),
        )
        .unwrap();
        let pending = finish(&mut Scanner::new(Kind::Ack), &config).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(!format!("{:?}", pending[0]).contains("private-key"));
        assert_same_as_existing_reader(Kind::Ack, &config, pending);
        for version in [1, 2] {
            let (_dir, config) = fixture();
            let mut bad = intent(Kind::Ack, 1);
            bad["schema_version"] = json!(version);
            bad["idempotency"] = json!({"key": "missing-fingerprint"});
            append(&config, Kind::Ack, &seal(bad, true));
            assert!(finish(&mut Scanner::new(Kind::Ack), &config).is_err());
        }
    }

    #[test]
    fn invalid_hashes_never_authorize_an_intent_or_a_completion() {
        for kind in [Kind::Ack, Kind::Release] {
            let (_dir, config) = fixture();
            let first = intent(kind, 1);
            append(&config, kind, &first);
            let mut bad = intent(kind, 2);
            bad["content_sha256"] = json!("0".repeat(64));
            append(&config, kind, &bad);
            let mut bad = marker(kind, &first, journal::REPLAY_STATUS_REPLAYED);
            bad["content_sha256"] = json!("0".repeat(64));
            append(&config, kind, &bad);
            let pending = finish(&mut Scanner::new(kind), &config).unwrap();
            assert_eq!(pending.len(), 1);
            assert_same_as_existing_reader(kind, &config, pending);
        }
    }

    #[test]
    fn snapshot_pins_eof_without_chasing_a_concurrent_appender() {
        let (_dir, config) = fixture();
        append(&config, Kind::Ack, &intent(Kind::Ack, 1));
        let mut scanner = Scanner::new(Kind::Ack);
        assert!(
            scanner
                .poll_bounded(&config, &AtomicBool::new(false), 1, 1)
                .unwrap()
                .is_none()
        );
        append(&config, Kind::Ack, &intent(Kind::Ack, 2));
        assert_eq!(finish(&mut scanner, &config).unwrap().len(), 1);
        assert_eq!(finish(&mut scanner, &config).unwrap().len(), 2);
    }

    #[test]
    fn cancelled_partial_snapshot_releases_state_and_restarts_safely() {
        let (_dir, config) = fixture();
        append(&config, Kind::Ack, &intent(Kind::Ack, 1));
        let mut scanner = Scanner::new(Kind::Ack);
        let stop = AtomicBool::new(false);
        assert!(
            scanner
                .poll_bounded(&config, &stop, 1, 1)
                .unwrap()
                .is_none()
        );
        stop.store(true, Ordering::Release);
        assert_eq!(
            scanner.poll(&config, &stop).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(scanner.snapshot.is_none());
        assert_eq!(finish(&mut scanner, &config).unwrap().len(), 1);
    }

    #[test]
    fn replaced_and_truncated_journals_cannot_publish_partial_queues() {
        for replace in [true, false] {
            let (_dir, config) = fixture();
            append(&config, Kind::Ack, &intent(Kind::Ack, 1));
            let path = journal::log_path(&config, Kind::Ack.file_name());
            let mut scanner = Scanner::new(Kind::Ack);
            assert!(
                scanner
                    .poll_bounded(&config, &AtomicBool::new(false), 1, 1)
                    .unwrap()
                    .is_none()
            );
            if replace {
                std::fs::rename(&path, config.storage_root.join("retained-journal.jsonl")).unwrap();
                append(&config, Kind::Ack, &intent(Kind::Ack, 2));
            } else {
                // Fixture models an external truncation; production never truncates.
                std::fs::copy(&path, config.storage_root.join("retained-journal.jsonl")).unwrap();
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(0)
                    .unwrap();
            }
            assert!(finish(&mut scanner, &config).is_err());
            assert!(scanner.snapshot.is_none());
        }
    }

    #[test]
    fn all_admission_limits_fail_explicitly_without_returning_a_prefix() {
        for bound in ["snapshot", "record", "pending", "identities"] {
            let (_dir, config) = fixture();
            append(&config, Kind::Ack, &intent(Kind::Ack, 1));
            append(&config, Kind::Ack, &intent(Kind::Ack, 2));
            let path = journal::log_path(&config, Kind::Ack.file_name());
            let before = std::fs::read(&path).unwrap();
            let mut scanner = Scanner::new(Kind::Ack);
            match bound {
                "snapshot" => scanner.limits.snapshot_bytes = 1,
                "record" => scanner.limits.record_bytes = 1,
                "pending" => scanner.limits.pending_bytes = 1,
                "identities" => scanner.limits.identities = 1,
                _ => unreachable!(),
            }
            assert!(finish(&mut scanner, &config).is_err(), "{bound}");
            assert!(scanner.snapshot.is_none());
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
    }

    #[test]
    fn duplicate_records_do_not_consume_identity_or_payload_budgets_twice() {
        let (_dir, config) = fixture();
        let first = intent(Kind::Ack, 1);
        for _ in 0..3 {
            append(&config, Kind::Ack, &first);
        }
        append(
            &config,
            Kind::Ack,
            &marker(Kind::Ack, &first, journal::REPLAY_STATUS_REPLAYED),
        );
        append(&config, Kind::Ack, &first);
        let mut scanner = Scanner::new(Kind::Ack);
        scanner.limits.identities = 1;
        scanner.limits.pending_bytes = serde_json::to_vec(&first).unwrap().len() + 1;
        assert!(finish(&mut scanner, &config).unwrap().is_empty());
    }

    #[test]
    fn torn_json_and_utf8_are_isolated_across_single_byte_polls() {
        let (_dir, config) = fixture();
        append(&config, Kind::Ack, &intent(Kind::Ack, 1));
        let path = journal::log_path(&config, Kind::Ack.file_name());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{torn\n\xff\xfe\n").unwrap();
        drop(file);
        append(&config, Kind::Ack, &intent(Kind::Ack, 2));
        let mut scanner = Scanner::new(Kind::Ack);
        let mut result = None;
        for _ in 0..10_000 {
            result = scanner
                .poll_bounded(&config, &AtomicBool::new(false), 1, 1)
                .unwrap();
            if result.is_some() {
                break;
            }
        }
        let pending = result.expect("bounded scan reaches EOF");
        assert_eq!(pending.len(), 2);
        assert_same_as_existing_reader(Kind::Ack, &config, pending);
    }

    #[test]
    fn missing_sources_and_zero_budgets_never_create_or_consume_journals() {
        let (_dir, config) = fixture();
        assert!(
            finish(&mut Scanner::new(Kind::Ack), &config)
                .unwrap()
                .is_empty()
        );
        assert!(
            !config
                .storage_root
                .join(journal::DEGRADED_INTENTS_DIR)
                .exists()
        );
        append(&config, Kind::Ack, &intent(Kind::Ack, 1));
        let mut scanner = Scanner::new(Kind::Ack);
        for (bytes, records) in [(0, 1), (1, 0)] {
            assert!(
                scanner
                    .poll_bounded(&config, &AtomicBool::new(false), bytes, records)
                    .unwrap()
                    .is_none()
            );
            let snapshot = scanner.snapshot.as_ref().unwrap();
            assert_eq!(snapshot.reader.get_ref().limit(), snapshot.initial_bytes);
            assert_eq!(snapshot.line, [] as [u8; 0]);
        }
        assert_eq!(finish(&mut scanner, &config).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_journal_is_refused_without_touching_its_target() {
        let (_dir, config) = fixture();
        append(&config, Kind::Ack, &intent(Kind::Ack, 1));
        let path = journal::log_path(&config, Kind::Ack.file_name());
        let retained = config.storage_root.join("retained-journal.jsonl");
        std::fs::rename(&path, &retained).unwrap();
        let before = std::fs::read(&retained).unwrap();
        std::os::unix::fs::symlink(&retained, &path).unwrap();
        assert!(finish(&mut Scanner::new(Kind::Ack), &config).is_err());
        assert_eq!(std::fs::read_link(path).unwrap(), retained);
        assert_eq!(std::fs::read(retained).unwrap(), before);
    }
}
