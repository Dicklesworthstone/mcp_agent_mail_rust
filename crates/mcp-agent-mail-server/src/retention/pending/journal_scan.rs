//! Incremental, read-only admission of durable closeout journals.
//!
//! Nothing is eligible for replay until the entire pinned EOF has been checked:
//! a terminal marker can follow its intent, or precede a duplicate of it. Each
//! poll consumes bounded bytes/records and revalidates the retained file handle.
//! Candidate windows are checked against the same complete snapshot before
//! unresolved payloads are admitted. Memory is bounded by unresolved work and
//! one window, not the journal's completed history. Unsupported records or live
//! queue overload remain errors. This reader never rewrites journal evidence.

use std::collections::HashMap;
use std::io::{self, BufRead, Seek, SeekFrom};
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
    candidate_bytes: usize,
    candidate_identities: usize,
    pending_bytes: usize,
    identities: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            record_bytes: MAX_RECORD_BYTES,
            candidate_bytes: MAX_PENDING_BYTES,
            candidate_identities: MAX_IDENTITIES,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Candidates,
    Terminals,
    Admit,
}

struct Snapshot {
    kind: Kind,
    directory: JournalDirectory,
    reader: io::BufReader<std::fs::File>,
    initial_bytes: u64,
    position: u64,
    line_start: u64,
    line: Vec<u8>,
    phase: Phase,
    resume_at: u64,
    candidates: HashMap<Key, (u64, Intent, usize)>,
    candidate_bytes: usize,
    admission: Option<std::collections::hash_map::IntoIter<Key, (u64, Intent, usize)>>,
    pending: HashMap<Key, (u64, Intent, usize)>,
    pending_bytes: usize,
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
        Ok(Some(Self {
            kind,
            directory,
            reader: io::BufReader::with_capacity(8192, file),
            initial_bytes,
            position: 0,
            line_start: 0,
            line: Vec::new(),
            phase: Phase::Candidates,
            resume_at: 0,
            candidates: HashMap::new(),
            candidate_bytes: 0,
            admission: None,
            pending: HashMap::new(),
            pending_bytes: 0,
            skipped: 0,
            limits,
        }))
    }

    fn validate(&self) -> io::Result<()> {
        let file = self.reader.get_ref();
        self.directory.validate_file(self.kind.file_name(), file)?;
        if file.metadata()?.len() < self.initial_bytes {
            return Err(invalid(
                "closeout journal truncated during snapshot admission; partial queue refused",
            ));
        }
        Ok(())
    }

    fn seek_to(&mut self, position: u64) -> io::Result<()> {
        self.validate()?;
        // BufReader::seek discards prefetched bytes. Keep the original handle
        // and EOF: no phase may reopen a different source or chase appends.
        self.reader.seek(SeekFrom::Start(position))?;
        self.position = position;
        self.line_start = position;
        self.line.clear();
        Ok(())
    }

    fn validate_candidates(&mut self, resume_at: u64) -> io::Result<()> {
        self.resume_at = resume_at;
        self.phase = Phase::Terminals;
        self.seek_to(0)
    }

    fn advance(
        &mut self,
        shutdown: &AtomicBool,
        mut bytes: usize,
        mut records: usize,
    ) -> io::Result<bool> {
        while bytes > 0 && records > 0 {
            check_shutdown(shutdown)?;
            if self.phase == Phase::Admit {
                let next = self.admission.as_mut().and_then(Iterator::next);
                if let Some((key, entry)) = next {
                    self.admit_pending(key, entry)?;
                    records -= 1;
                } else {
                    self.admission = None;
                    self.phase = Phase::Candidates;
                    self.seek_to(self.resume_at)?;
                    if self.position == self.initial_bytes {
                        return Ok(true);
                    }
                }
                continue;
            }
            if self.position == self.initial_bytes {
                if !self.line.is_empty() {
                    self.accept_line()?;
                    records -= 1;
                    continue;
                }
                match self.phase {
                    Phase::Candidates if self.candidates.is_empty() => return Ok(true),
                    Phase::Candidates => self.validate_candidates(self.position)?,
                    Phase::Terminals => {
                        self.admission = Some(std::mem::take(&mut self.candidates).into_iter());
                        self.phase = Phase::Admit;
                    }
                    Phase::Admit => unreachable!("handled before reading"),
                }
                continue;
            }
            let remaining =
                usize::try_from(self.initial_bytes - self.position).unwrap_or(usize::MAX);
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "closeout journal truncated before its pinned EOF",
                ));
            }
            let available = &available[..available.len().min(bytes).min(remaining)];
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            if consumed > self.limits.record_bytes.saturating_sub(self.line.len()) {
                return Err(invalid(
                    "closeout journal record exceeds its byte limit; partial queue refused",
                ));
            }
            self.line.extend_from_slice(&available[..consumed]);
            self.reader.consume(consumed);
            self.position += u64::try_from(consumed).expect("bounded read fits its snapshot");
            bytes -= consumed;
            if newline.is_some() {
                self.accept_line()?;
                records -= 1;
            }
        }
        Ok(false)
    }

    fn accept_line(&mut self) -> io::Result<()> {
        let bytes = self.line.len();
        let start = self.line_start;
        let decoded = if self.line.iter().all(u8::is_ascii_whitespace) {
            None
        } else if let Ok(value) = serde_json::from_slice::<Value>(&self.line) {
            // Every phase validates schemas and hashes, including records
            // unrelated to the selected candidates. No prefix is replayable.
            Some(decode(self.kind, value)?)
        } else {
            if self.phase == Phase::Candidates {
                self.skipped = self.skipped.saturating_add(1);
            }
            None
        };
        self.line.clear();
        self.line_start = self.position;
        match (self.phase, decoded) {
            (Phase::Candidates, Some(Record::Intent(intent))) => {
                let key = intent.key();
                if self.pending.contains_key(&key) || self.candidates.contains_key(&key) {
                    return Ok(());
                }
                if bytes > self.limits.candidate_bytes || self.limits.candidate_identities == 0 {
                    return Err(invalid(
                        "closeout journal candidate cannot fit its window; partial queue refused",
                    ));
                }
                if self.candidates.len() >= self.limits.candidate_identities
                    || bytes
                        > self
                            .limits
                            .candidate_bytes
                            .saturating_sub(self.candidate_bytes)
                {
                    // This complete record belongs to the next window. Resume
                    // at its start, never after an intent that was not retained.
                    return self.validate_candidates(start);
                }
                self.candidate_bytes += bytes;
                self.candidates.insert(key, (start, intent, bytes));
                if self.candidates.len() == self.limits.candidate_identities
                    || self.candidate_bytes == self.limits.candidate_bytes
                {
                    self.validate_candidates(self.position)?;
                }
            }
            (Phase::Terminals, Some(Record::Terminal(key))) => {
                if let Some((_, _, bytes)) = self.candidates.remove(&key) {
                    self.candidate_bytes -= bytes;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn admit_pending(&mut self, key: Key, entry: (u64, Intent, usize)) -> io::Result<()> {
        // Survivors have now been checked against every terminal marker in
        // this same pinned snapshot, including markers before their intents.
        // Limit unresolved work, never the journal's completed lifetime history.
        if self.pending.len() >= self.limits.identities {
            return Err(invalid(
                "closeout journal pending identity limit exceeded; partial queue refused",
            ));
        }
        let bytes = entry.2;
        if bytes > self.limits.pending_bytes.saturating_sub(self.pending_bytes) {
            return Err(invalid(
                "closeout journal pending payload limit exceeded; partial queue refused",
            ));
        }
        self.candidate_bytes -= bytes;
        self.pending_bytes += bytes;
        self.pending.insert(key, entry);
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
                scanner.limits.candidate_identities = 1;
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
        for bound in ["record", "pending", "identities"] {
            let (_dir, config) = fixture();
            append(&config, Kind::Ack, &intent(Kind::Ack, 1));
            append(&config, Kind::Ack, &intent(Kind::Ack, 2));
            let path = journal::log_path(&config, Kind::Ack.file_name());
            let before = std::fs::read(&path).unwrap();
            let mut scanner = Scanner::new(Kind::Ack);
            scanner.limits.candidate_identities = 1;
            match bound {
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
    fn completed_history_exceeding_scaled_snapshot_and_identity_budgets_is_replayable() {
        for kind in [Kind::Ack, Kind::Release] {
            let (_dir, config) = fixture();
            let live = intent(kind, 1000);
            append(&config, kind, &live);
            for sequence in 1..=12 {
                let done = intent(kind, sequence);
                if sequence % 2 == 0 {
                    append(
                        &config,
                        kind,
                        &marker(kind, &done, journal::REPLAY_STATUS_REPLAYED),
                    );
                }
                append(&config, kind, &done);
                if sequence % 2 != 0 {
                    append(
                        &config,
                        kind,
                        &marker(kind, &done, journal::REPLAY_STATUS_ABANDONED),
                    );
                }
            }
            append(&config, kind, &live);
            let mut scanner = Scanner::new(kind);
            scanner.limits.identities = 1;
            scanner.limits.pending_bytes = serde_json::to_vec(&live).unwrap().len() + 1;
            scanner.limits.candidate_identities = 2;
            scanner.limits.candidate_bytes = scanner.limits.pending_bytes * 2;
            let path = journal::log_path(&config, kind.file_name());
            let before = std::fs::read(&path).unwrap();
            // The former snapshot cap was four times the pending-byte cap.
            // Scale both budgets down so this exercises a larger lifetime
            // history without allocating a 64 MiB fixture.
            assert!(before.len() > scanner.limits.pending_bytes * 4);
            let pending = finish(&mut scanner, &config).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].key().0, live["intent_id"].as_str().unwrap());
            assert_same_as_existing_reader(kind, &config, pending);
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
    }

    #[test]
    fn terminal_before_duplicates_is_checked_across_windows_in_first_append_order() {
        for kind in [Kind::Ack, Kind::Release] {
            let (_dir, config) = fixture();
            let completed = intent(kind, 1);
            let first = intent(kind, 99);
            let second = intent(kind, 2);
            append(
                &config,
                kind,
                &marker(kind, &completed, journal::REPLAY_STATUS_REPLAYED),
            );
            for record in [&first, &completed, &second, &completed, &first] {
                append(&config, kind, record);
            }
            let mut scanner = Scanner::new(kind);
            scanner.limits.candidate_identities = 1;
            scanner.limits.identities = 2;
            let pending = finish(&mut scanner, &config).unwrap();
            let ids: Vec<_> = pending.iter().map(|intent| intent.key().0).collect();
            assert_eq!(
                ids,
                [
                    first["intent_id"].as_str().unwrap(),
                    second["intent_id"].as_str().unwrap(),
                ]
            );
            assert_same_as_existing_reader(kind, &config, pending);
        }
    }

    #[test]
    fn candidate_byte_overflow_revisits_whole_records_including_unterminated_tail() {
        for kind in [Kind::Ack, Kind::Release] {
            for newline in [true, false] {
                let (_dir, config) = fixture();
                let first = intent(kind, 1);
                let second = intent(kind, 1000);
                let first_payload = serde_json::to_vec(&first).unwrap();
                let second_payload = serde_json::to_vec(&second).unwrap();
                append(&config, kind, &first);
                if newline {
                    append(&config, kind, &second);
                } else {
                    // A complete valid JSON record may survive a crash without
                    // its newline. It still participates in both scan passes.
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(journal::log_path(&config, kind.file_name()))
                        .unwrap()
                        .write_all(&second_payload)
                        .unwrap();
                }
                let mut scanner = Scanner::new(kind);
                scanner.limits.candidate_bytes = first_payload.len().max(second_payload.len()) + 1;
                scanner.limits.candidate_identities = 64;
                let first_bytes = first_payload.len() + 1;
                assert!(
                    first_bytes < scanner.limits.candidate_bytes,
                    "the first record must leave a nonzero, insufficient remainder"
                );
                let stop = AtomicBool::new(false);
                assert!(
                    scanner
                        .poll_bounded(&config, &stop, usize::MAX, 1)
                        .unwrap()
                        .is_none()
                );
                assert_eq!(scanner.snapshot.as_ref().unwrap().phase, Phase::Candidates);
                assert!(
                    scanner
                        .poll_bounded(&config, &stop, usize::MAX, 1)
                        .unwrap()
                        .is_none()
                );
                let snapshot = scanner.snapshot.as_ref().unwrap();
                assert_eq!(snapshot.phase, Phase::Terminals);
                assert_eq!(snapshot.position, 0);
                assert_eq!(snapshot.resume_at, u64::try_from(first_bytes).unwrap());
                assert_eq!(snapshot.candidates.len(), 1);
                let pending = finish(&mut scanner, &config).unwrap();
                assert_eq!(
                    pending
                        .iter()
                        .map(|intent| intent.key().0)
                        .collect::<Vec<_>>(),
                    [
                        first["intent_id"].as_str().unwrap(),
                        second["intent_id"].as_str().unwrap(),
                    ]
                );
                assert_same_as_existing_reader(kind, &config, pending);
            }
        }
    }

    #[test]
    fn phase_changes_preserve_each_polls_byte_and_record_budgets() {
        let (_dir, config) = fixture();
        let first = intent(Kind::Ack, 1);
        append(&config, Kind::Ack, &first);
        append(&config, Kind::Ack, &intent(Kind::Ack, 2));
        let first_bytes = serde_json::to_vec(&first).unwrap().len() + 1;
        let stop = AtomicBool::new(false);
        for (bytes, records) in [(first_bytes, 10), (usize::MAX, 1)] {
            let mut scanner = Scanner::new(Kind::Ack);
            scanner.limits.candidate_identities = 1;
            assert!(
                scanner
                    .poll_bounded(&config, &stop, bytes, records)
                    .unwrap()
                    .is_none()
            );
            let snapshot = scanner.snapshot.as_ref().unwrap();
            assert_eq!(snapshot.phase, Phase::Terminals);
            assert_eq!(snapshot.position, 0, "the new pass gets no renewed budget");
            assert_eq!(snapshot.candidates.len(), 1);
            assert!(
                scanner
                    .poll_bounded(&config, &stop, 1, 10)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(scanner.snapshot.as_ref().unwrap().position, 1);
            assert_eq!(finish(&mut scanner, &config).unwrap().len(), 2);
        }

        let mut scanner = Scanner::new(Kind::Ack);
        let file_bytes = std::fs::metadata(journal::log_path(&config, Kind::Ack.file_name()))
            .unwrap()
            .len();
        // Stop exactly at the validation EOF. Both survivors still need
        // admission, and that in-memory work consumes the same record budget.
        for _ in 0..2 {
            assert!(
                scanner
                    .poll_bounded(&config, &stop, usize::try_from(file_bytes).unwrap(), 10,)
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            scanner
                .poll_bounded(&config, &stop, usize::MAX, 1)
                .unwrap()
                .is_none()
        );
        let snapshot = scanner.snapshot.as_ref().unwrap();
        assert_eq!(snapshot.phase, Phase::Admit);
        assert_eq!(
            snapshot.pending.len(),
            1,
            "admission cannot merge a whole window at once"
        );
        assert_eq!(finish(&mut scanner, &config).unwrap().len(), 2);
    }

    #[test]
    fn every_validation_pass_uses_the_original_eof_despite_new_completions() {
        for kind in [Kind::Ack, Kind::Release] {
            let (_dir, config) = fixture();
            let first = intent(kind, 1);
            let second = intent(kind, 2);
            let later = intent(kind, 3);
            append(&config, kind, &first);
            append(&config, kind, &second);
            let mut scanner = Scanner::new(kind);
            scanner.limits.candidate_identities = 1;
            assert!(
                scanner
                    .poll_bounded(&config, &AtomicBool::new(false), usize::MAX, 1)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(scanner.snapshot.as_ref().unwrap().phase, Phase::Terminals);
            append(
                &config,
                kind,
                &marker(kind, &first, journal::REPLAY_STATUS_REPLAYED),
            );
            append(&config, kind, &first);
            append(&config, kind, &later);
            let pinned = finish(&mut scanner, &config).unwrap();
            assert_eq!(
                pinned
                    .iter()
                    .map(|intent| intent.key().0)
                    .collect::<Vec<_>>(),
                [
                    first["intent_id"].as_str().unwrap(),
                    second["intent_id"].as_str().unwrap(),
                ]
            );
            let current = finish(&mut scanner, &config).unwrap();
            assert_eq!(current.len(), 2);
            assert_same_as_existing_reader(kind, &config, current);
        }
    }

    #[test]
    fn cancellation_and_source_changes_discard_already_validated_windows() {
        for failure in ["cancel", "replace", "truncate"] {
            let (_dir, config) = fixture();
            append(&config, Kind::Ack, &intent(Kind::Ack, 1));
            let mut scanner = Scanner::new(Kind::Ack);
            scanner.limits.candidate_identities = 1;
            let stop = AtomicBool::new(false);
            for _ in 0..3 {
                assert!(
                    scanner
                        .poll_bounded(&config, &stop, usize::MAX, 1)
                        .unwrap()
                        .is_none()
                );
            }
            let snapshot = scanner.snapshot.as_ref().unwrap();
            assert_eq!(snapshot.phase, Phase::Admit);
            assert_eq!(snapshot.pending.len(), 1);
            let path = journal::log_path(&config, Kind::Ack.file_name());
            let retained = config.storage_root.join("retained-journal.jsonl");
            match failure {
                "cancel" => stop.store(true, Ordering::Release),
                "replace" => {
                    std::fs::rename(&path, &retained).unwrap();
                    append(&config, Kind::Ack, &intent(Kind::Ack, 2));
                }
                "truncate" => {
                    std::fs::copy(&path, &retained).unwrap();
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_len(0)
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert!(scanner.poll(&config, &stop).is_err(), "{failure}");
            assert!(scanner.snapshot.is_none());
            stop.store(false, Ordering::Release);
            let pending = finish(&mut scanner, &config).unwrap();
            assert_eq!(pending.len(), usize::from(failure != "truncate"));
            assert_same_as_existing_reader(Kind::Ack, &config, pending);
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
            assert_eq!(snapshot.position, 0);
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
