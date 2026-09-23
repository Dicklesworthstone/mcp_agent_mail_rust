//! Shared durable-intent ("degraded mode") primitives (br-bvq1x.8.3 / H3).
//!
//! When a mutating tool cannot reach the live mailbox (DB corrupt / locked /
//! busy / pool-exhausted / circuit-open), it records a hash-witnessed JSONL
//! "intent" under `<storage_root>/degraded_intents/` so the action is never
//! silently dropped and can be replayed once the mailbox is healthy. This is
//! the single home for the security-sensitive on-disk primitives (symlink
//! rejection, restrictive perms, content hashing, replay de-duplication)
//! shared across intent kinds.
//!
//! ## Intent log layout (one JSONL file per verb)
//!
//! - `release_file_reservations.jsonl` — release intents (writer lives in
//!   [`crate::reservations`]; this module exposes a read-only view used by the
//!   `am robot status` surface).
//! - `acknowledge_message.jsonl` — ack intents (full writer + reader live
//!   here, used by [`crate::messaging::acknowledge_message`]).
//!
//! Each file interleaves two record kinds:
//! - an **intent** record (`kind == "<verb>_intent"`) appended when the action
//!   was queued, carrying a `content_sha256` over its canonical payload and a
//!   16-char `intent_id` prefix of that hash; and
//! - a **replay** marker (`kind == "<verb>_replay"`) appended after replay,
//!   with `status` (`"replayed"/"failed"`), `intent_id`, and
//!   `intent_content_sha256`.
//!
//! A queued intent is outstanding until a terminal replay marker referencing
//! its full `(intent_id, content_sha256)` pair is present. Readers isolate torn
//! JSON/UTF-8 records without losing intact records on either side. They read
//! one opened file up to its initial length, never chase a concurrent appender.
//! Completed payloads are dropped during the scan; only their terminal keys
//! remain to prevent a later duplicate from resurrecting completed work.
//! Publication and snapshot completion revalidate retained directory/file
//! handles: an observed replacement is not a durable receipt or an empty queue.
//!
//! NOTE: [`crate::reservations`] still carries its own private copy of the
//! release-intent writer/reader for its automatic replay-on-success path.
//! Migrating it onto this shared surface is tracked as a follow-up so that the
//! single mutation chokepoint work (Track F) is not disturbed.

use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::io::{BufRead as _, Read as _, Write as _};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::journal_io::{JournalDirectory, JournalFileMode, validate_entry_name};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Maximum bytes in one JSONL record, including its terminating newline.
/// Oversized historical records are an explicit error, never silently omitted.
const MAX_INTENT_RECORD_BYTES: usize = 16 * 1024 * 1024;
/// The mailbox has already failed when this fallback is used. Bound contention
/// on its separate journal lock instead of introducing a second indefinite wait.
/// This is a lock-wait budget, not a deadline for filesystem I/O or `fsync`.
const INTENT_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const INTENT_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(5);

/// Subdirectory under `storage_root` holding every degraded-intent log.
pub const DEGRADED_INTENTS_DIR: &str = "degraded_intents";

/// Release-intent log filename (mirrors the private constant in
/// [`crate::reservations`]; kept here for the read-only robot surface).
pub const RELEASE_INTENT_LOG_FILE: &str = "release_file_reservations.jsonl";
/// Release-intent record kind.
pub const RELEASE_INTENT_KIND: &str = "release_file_reservations_intent";
/// Release-intent replay marker kind.
pub const RELEASE_INTENT_REPLAY_KIND: &str = "release_file_reservations_replay";

/// Original unkeyed ack-intent schema version.
pub const ACK_INTENT_SCHEMA_VERSION: u32 = 1;
/// Keyed ack-intent schema version. Its hash includes the retry claim, so an
/// older reader cannot silently replay it as an unkeyed acknowledgement.
pub const KEYED_ACK_INTENT_SCHEMA_VERSION: u32 = 2;
/// Ack-intent log filename.
pub const ACK_INTENT_LOG_FILE: &str = "acknowledge_message.jsonl";
/// Ack-intent advisory lock filename.
pub const ACK_INTENT_LOCK_FILE: &str = ".acknowledge_message.jsonl.lock";
/// Ack-intent record kind.
pub const ACK_INTENT_KIND: &str = "acknowledge_message_intent";
/// Ack-intent replay marker kind.
pub const ACK_INTENT_REPLAY_KIND: &str = "acknowledge_message_replay";

/// Replay-marker `status` after a successful replay (terminal — clears intent).
pub const REPLAY_STATUS_REPLAYED: &str = "replayed";
/// Replay-marker `status` after a transient/corruption failure (intent stays
/// queued and is retried on the next successful call).
pub const REPLAY_STATUS_FAILED: &str = "failed";
/// Replay-marker `status` after a permanent, non-retryable failure such as the
/// target message no longer existing (terminal — clears the intent so it does
/// not accumulate forever).
pub const REPLAY_STATUS_ABANDONED: &str = "abandoned";

/// Whether a replay `status` is terminal (clears the queued intent).
#[must_use]
pub fn is_terminal_replay_status(status: &str) -> bool {
    matches!(status, REPLAY_STATUS_REPLAYED | REPLAY_STATUS_ABANDONED)
}

/// Failure context captured alongside a queued intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentFailure {
    /// Pipeline stage where the live path failed (e.g. `"resolve_agent"`).
    pub stage: String,
    /// Best-effort detail string from the underlying error.
    pub error_detail: String,
}

/// The exact normalized retry claim supplied with a queued acknowledgement.
///
/// Key and fingerprint travel together: a partial claim must never degrade to
/// an unkeyed mutation. The raw key is written only to the private intent log;
/// diagnostics and robot summaries must not expose it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AckIntentIdempotency {
    /// Original normalized client key.
    pub key: String,
    /// Original normalized request fingerprint, preserved across restarts.
    pub fingerprint: String,
}

impl std::fmt::Debug for AckIntentIdempotency {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AckIntentIdempotency")
            .finish_non_exhaustive()
    }
}

impl AckIntentIdempotency {
    fn is_valid(&self) -> bool {
        !self.key.is_empty()
            && self.key.trim() == self.key
            && self.key.chars().count() <= crate::idempotency::MAX_IDEMPOTENCY_KEY_LEN
            && self.fingerprint.len() == 64
            && self
                .fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }
}

/// A queued (un-replayed) acknowledge-message intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedAckIntent {
    /// Schema version of the on-disk record.
    pub schema_version: u32,
    /// Record kind; always [`ACK_INTENT_KIND`].
    pub kind: String,
    /// 16-char content-hash prefix uniquely identifying this intent.
    pub intent_id: String,
    /// Full SHA-256 over the canonical payload.
    pub content_sha256: String,
    /// Creation time (microseconds since epoch).
    pub created_ts: i64,
    /// Project key the ack targeted.
    pub project_key: String,
    /// Agent that attempted the ack.
    pub agent_name: String,
    /// Message id that was being acknowledged.
    pub message_id: i64,
    /// Original retry claim. Omitted from generic diagnostic serialization;
    /// the durable writer explicitly includes it in the private hashed record.
    #[serde(default, skip_serializing)]
    pub idempotency: Option<AckIntentIdempotency>,
    /// Failure context that forced queuing.
    pub failure: IntentFailure,
}

/// A queued (un-replayed) release-file-reservations intent (read-only view).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedReleaseIntentView {
    /// 16-char content-hash prefix uniquely identifying this intent.
    pub intent_id: String,
    /// Full SHA-256 over the canonical payload.
    pub content_sha256: String,
    /// Creation time (microseconds since epoch).
    pub created_ts: i64,
    /// Project key the release targeted.
    pub project_key: String,
    /// Agent that attempted the release.
    pub agent_name: String,
    /// Optional path patterns the release targeted.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Optional reservation ids the release targeted.
    #[serde(default)]
    pub file_reservation_ids: Option<Vec<i64>>,
}

/// Receipt returned after appending an intent.
#[derive(Debug, Clone)]
pub struct IntentReceipt {
    /// 16-char content-hash prefix uniquely identifying this intent.
    pub intent_id: String,
    /// Absolute path of the intent log the record was appended to.
    pub intent_path: PathBuf,
    /// Full SHA-256 over the canonical payload.
    pub content_sha256: String,
}

/// Absolute path of an intent log file.
#[must_use]
pub fn log_path(config: &Config, file_name: &str) -> PathBuf {
    config
        .storage_root
        .join(DEGRADED_INTENTS_DIR)
        .join(file_name)
}

#[cfg(test)]
fn lock_path(config: &Config, lock_file_name: &str) -> PathBuf {
    config
        .storage_root
        .join(DEGRADED_INTENTS_DIR)
        .join(lock_file_name)
}

/// Stable SHA-256 (hex) over a JSON value.
#[must_use]
pub fn hash_json_value(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).expect("serializing serde_json::Value should not fail");
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Fixture setup only; production uses retained journal handles.
#[cfg(test)]
fn reject_existing_symlink(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::other(format!(
            "degraded-intent path must not be a symlink: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
fn ensure_intent_parent(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::other("degraded-intent log has no parent"));
    };
    reject_existing_symlink(parent)?;
    std::fs::create_dir_all(parent)?;
    reject_existing_symlink(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn is_lock_contention(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error
            .raw_os_error()
            .is_some_and(|code| Some(code) == fs2::lock_contended_error().raw_os_error())
}

fn intent_lock_timeout(timeout: Duration) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("degraded-intent journal lock exceeded {timeout:?}; intent was not appended"),
    )
}

/// Retry only contention/interruption, under one monotonic budget. A zero
/// budget still permits one nonblocking attempt, but never waits. The caller
/// keeps the owned file alive through publication; dropping it releases the lock.
fn acquire_intent_lock(file: &std::fs::File, timeout: Duration) -> std::io::Result<()> {
    let started = Instant::now();
    loop {
        match fs2::FileExt::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(error)
                if is_lock_contention(&error)
                    || error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(intent_lock_timeout(timeout));
        }
        std::thread::sleep(remaining.min(INTENT_LOCK_RETRY_INTERVAL));
        // Do not start a new acquisition attempt after the wait budget expired.
        if started.elapsed() >= timeout {
            return Err(intent_lock_timeout(timeout));
        }
    }
}

/// Append a single JSON record under an exclusive advisory lock, with private
/// permissions and file sync plus retained Unix directory sync.
///
/// Lock contention is bounded. A timeout or observed authority replacement is
/// an error, never a receipt claiming that the current journal queued the intent.
pub fn append_jsonl(
    config: &Config,
    file_name: &str,
    lock_file_name: &str,
    record: &Value,
) -> std::io::Result<PathBuf> {
    // Do not acknowledge a record that the bounded reader cannot recover.
    let payload =
        serde_json::to_vec(record).map_err(|error| std::io::Error::other(error.to_string()))?;
    if payload.len() >= MAX_INTENT_RECORD_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "degraded-intent record exceeds its byte limit",
        ));
    }
    validate_entry_name(file_name)?;
    validate_entry_name(lock_file_name)?;
    if file_name.eq_ignore_ascii_case(lock_file_name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "degraded-intent lock and log must have distinct names",
        ));
    }
    let directory = JournalDirectory::open(&config.storage_root, DEGRADED_INTENTS_DIR, true)?;
    let path = directory.directory_path().join(file_name);
    let lock_file = directory.open_file(lock_file_name, JournalFileMode::Lock)?;
    acquire_intent_lock(&lock_file, INTENT_LOCK_TIMEOUT)?;
    // A renamed/replaced lock no longer excludes appenders using its new name.
    directory.validate_file(lock_file_name, &lock_file)?;
    let mut file = directory.open_file(file_name, JournalFileMode::Append)?;
    let metadata = file.metadata()?;
    // Isolate a torn prior append. Failure to inspect the tail must propagate,
    // not masquerade as an empty log and join two records into an unreadable one.
    let needs_leading_newline = if metadata.len() > 0 {
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        last[0] != b'\n'
    } else {
        false
    };
    let mut line = Vec::new();
    if needs_leading_newline {
        line.push(b'\n');
    }
    line.extend_from_slice(&payload);
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_all()?;
    directory.sync()?;
    directory.validate_file(lock_file_name, &lock_file)?;
    directory.validate_file(file_name, &file)?;
    Ok(path)
}

/// Byte-framed reader of a single append-only log snapshot. Parsing each record
/// independently is essential: a crash can tear a multibyte UTF-8 character,
/// not just JSON punctuation. Whole-file `read_to_string` loses every intact
/// record when even one such fragment exists anywhere in the log.
struct IntentLogReader {
    directory: JournalDirectory,
    file_name: String,
    reader: std::io::BufReader<std::io::Take<std::fs::File>>,
    line: Vec<u8>,
    record_limit: usize,
    skipped_records: u64,
    finished: bool,
}

impl IntentLogReader {
    fn open(config: &Config, file_name: &str) -> std::io::Result<Option<Self>> {
        validate_entry_name(file_name)?;
        let directory =
            match JournalDirectory::open(&config.storage_root, DEGRADED_INTENTS_DIR, false) {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
        let file = match directory.open_file(file_name, JournalFileMode::Read) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let snapshot_bytes = file.metadata()?.len();
        Ok(Some(Self {
            directory,
            file_name: file_name.to_string(),
            reader: std::io::BufReader::new(file.take(snapshot_bytes)),
            line: Vec::new(),
            record_limit: MAX_INTENT_RECORD_BYTES,
            skipped_records: 0,
            finished: false,
        }))
    }

    fn next_record(&mut self) -> std::io::Result<Option<Value>> {
        if self.finished {
            return Ok(None);
        }
        loop {
            self.line.clear();
            // Bound allocation BEFORE reading, including a delimiter-free line.
            let read = (&mut self.reader)
                .take(self.record_limit as u64 + 1)
                .read_until(b'\n', &mut self.line)?;
            if read == 0 {
                if self.reader.get_ref().limit() != 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "degraded-intent log was truncated during its snapshot read",
                    ));
                }
                // Public readers do not return their accumulated queue until
                // EOF. A detached/replaced snapshot must fail, not return a
                // misleading empty or partial view of the current journal.
                self.directory
                    .validate_file(&self.file_name, self.reader.get_ref().get_ref())?;
                self.finished = true;
                if self.skipped_records > 0 {
                    tracing::warn!(
                        skipped_records = self.skipped_records,
                        "ignored malformed degraded-intent records; intact records retained"
                    );
                }
                return Ok(None);
            }
            if self.line.len() > self.record_limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "degraded-intent record exceeds its byte limit; log preserved",
                ));
            }
            if self.line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice(&self.line) {
                Ok(value) => return Ok(Some(value)),
                Err(_) => self.skipped_records = self.skipped_records.saturating_add(1),
            }
        }
    }
}

type IntentKey = (String, String);

/// Retain live payloads, not the entire journal's historical payloads. Terminal
/// identities must remain: a marker can precede a duplicated intent record.
/// Memory still scales with the unresolved payloads and terminal-key history.
struct OutstandingIntents<T> {
    pending: HashMap<IntentKey, (u64, T)>,
    terminal: HashSet<IntentKey>,
    next_order: u64,
}

impl<T> OutstandingIntents<T> {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            terminal: HashSet::new(),
            next_order: 0,
        }
    }

    fn insert(&mut self, key: IntentKey, intent: T) -> std::io::Result<()> {
        if self.terminal.contains(&key) {
            return Ok(());
        }
        if let Entry::Vacant(entry) = self.pending.entry(key) {
            let order = self.next_order;
            self.next_order = order.checked_add(1).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "degraded-intent record sequence overflow",
                )
            })?;
            entry.insert((order, intent));
        }
        Ok(())
    }

    fn complete(&mut self, key: IntentKey) {
        // Drop the payload now, not at EOF. A later failed marker or duplicate
        // must not undo this verified terminal state.
        let _ = self.pending.remove(&key);
        self.terminal.insert(key);
    }

    fn into_intents(self) -> Vec<T> {
        let mut pending: Vec<_> = self.pending.into_values().collect();
        pending.sort_unstable_by_key(|(order, _)| *order);
        pending.into_iter().map(|(_, intent)| intent).collect()
    }
}

// ── Ack-intent canonical hashing ────────────────────────────────────────────

fn ack_intent_hash_payload(record: &Value) -> Value {
    let mut payload = json!({
        "schema_version": record["schema_version"].clone(),
        "kind": record["kind"].clone(),
        "created_ts": record["created_ts"].clone(),
        "project_key": record["project_key"].clone(),
        "agent_name": record["agent_name"].clone(),
        "message_id": record["message_id"].clone(),
        "failure": record["failure"].clone(),
    });
    if record["schema_version"].as_u64() == Some(u64::from(KEYED_ACK_INTENT_SCHEMA_VERSION)) {
        payload["idempotency"] = record["idempotency"].clone();
    }
    payload
}

fn ack_intent_has_supported_schema(record: &Value) -> bool {
    match record["schema_version"].as_u64() {
        Some(version) if version == u64::from(ACK_INTENT_SCHEMA_VERSION) => {
            // Reject an attached claim even when it was omitted from a v1
            // hash. It must never be accepted as an unkeyed instruction.
            record.get("idempotency").is_none()
        }
        Some(version) if version == u64::from(KEYED_ACK_INTENT_SCHEMA_VERSION) => record
            .get("idempotency")
            .and_then(|value| serde_json::from_value::<AckIntentIdempotency>(value.clone()).ok())
            .is_some_and(|claim| claim.is_valid()),
        _ => false,
    }
}

fn ack_replay_hash_payload(record: &Value) -> Value {
    json!({
        "schema_version": record["schema_version"].clone(),
        "kind": record["kind"].clone(),
        "intent_id": record["intent_id"].clone(),
        "intent_content_sha256": record["intent_content_sha256"].clone(),
        "replayed_ts": record["replayed_ts"].clone(),
        "status": record["status"].clone(),
        "error_detail": record["error_detail"].clone(),
    })
}

fn record_has_valid_intent_hash(record: &Value, hash_payload: fn(&Value) -> Value) -> bool {
    let Some(content_sha256) = record.get("content_sha256").and_then(Value::as_str) else {
        return false;
    };
    let Some(intent_id) = record.get("intent_id").and_then(Value::as_str) else {
        return false;
    };
    content_sha256.len() == 64
        && intent_id.len() == 16
        && content_sha256.starts_with(intent_id)
        && content_sha256 == hash_json_value(&hash_payload(record))
}

fn ack_replay_record_has_valid_hash(record: &Value) -> bool {
    if record["schema_version"].as_u64() != Some(u64::from(ACK_INTENT_SCHEMA_VERSION)) {
        return false;
    }
    let Some(content_sha256) = record.get("content_sha256").and_then(Value::as_str) else {
        return false;
    };
    let Some(intent_id) = record.get("intent_id").and_then(Value::as_str) else {
        return false;
    };
    let Some(intent_content_sha256) = record.get("intent_content_sha256").and_then(Value::as_str)
    else {
        return false;
    };
    content_sha256.len() == 64
        && intent_id.len() == 16
        && intent_content_sha256.len() == 64
        && intent_content_sha256.starts_with(intent_id)
        && content_sha256 == hash_json_value(&ack_replay_hash_payload(record))
}

/// Append a queued ack intent and return its receipt.
pub fn append_ack_intent(
    config: &Config,
    project_key: &str,
    agent_name: &str,
    message_id: i64,
    failure_stage: &str,
    error_detail: &str,
    idempotency: Option<&AckIntentIdempotency>,
) -> std::io::Result<IntentReceipt> {
    if idempotency.is_some_and(|claim| !claim.is_valid()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid acknowledgement idempotency claim",
        ));
    }
    let created_ts = mcp_agent_mail_db::now_micros();
    let schema_version = if idempotency.is_some() {
        KEYED_ACK_INTENT_SCHEMA_VERSION
    } else {
        ACK_INTENT_SCHEMA_VERSION
    };
    let mut payload = json!({
        "schema_version": schema_version,
        "kind": ACK_INTENT_KIND,
        "created_ts": created_ts,
        "project_key": project_key,
        "agent_name": agent_name,
        "message_id": message_id,
        "failure": {
            "stage": failure_stage,
            "error_detail": error_detail,
        },
    });
    if let Some(claim) = idempotency {
        payload["idempotency"] = json!(claim);
    }
    let content_sha256 = hash_json_value(&payload);
    let intent_id = content_sha256.chars().take(16).collect::<String>();
    let mut record = payload;
    record["intent_id"] = json!(intent_id);
    record["content_sha256"] = json!(content_sha256);
    let intent_path = append_jsonl(config, ACK_INTENT_LOG_FILE, ACK_INTENT_LOCK_FILE, &record)?;
    Ok(IntentReceipt {
        intent_id,
        intent_path,
        content_sha256,
    })
}

/// Append an ack-intent replay marker (best-effort; logs on failure).
pub fn append_ack_replay_record(
    config: &Config,
    intent_id: &str,
    intent_content_sha256: &str,
    status: &str,
    error_detail: Option<&str>,
) {
    let replayed_ts = mcp_agent_mail_db::now_micros();
    let payload = json!({
        "schema_version": ACK_INTENT_SCHEMA_VERSION,
        "kind": ACK_INTENT_REPLAY_KIND,
        "intent_id": intent_id,
        "intent_content_sha256": intent_content_sha256,
        "replayed_ts": replayed_ts,
        "status": status,
        "error_detail": error_detail,
    });
    let content_sha256 = hash_json_value(&payload);
    let record = json!({
        "schema_version": ACK_INTENT_SCHEMA_VERSION,
        "kind": ACK_INTENT_REPLAY_KIND,
        "intent_id": intent_id,
        "content_sha256": content_sha256,
        "intent_content_sha256": intent_content_sha256,
        "replayed_ts": replayed_ts,
        "status": status,
        "error_detail": error_detail,
    });
    if let Err(error) = append_jsonl(config, ACK_INTENT_LOG_FILE, ACK_INTENT_LOCK_FILE, &record) {
        tracing::warn!(
            error = %error,
            intent_id,
            "failed to append ack intent replay record"
        );
    }
}

/// Read all outstanding (un-replayed) ack intents in first-append order.
pub fn read_queued_ack_intents(config: &Config) -> std::io::Result<Vec<QueuedAckIntent>> {
    let Some(mut reader) = IntentLogReader::open(config, ACK_INTENT_LOG_FILE)? else {
        return Ok(Vec::new());
    };
    let mut outstanding = OutstandingIntents::new();
    while let Some(value) = reader.next_record()? {
        match value.get("kind").and_then(Value::as_str) {
            Some(ACK_INTENT_REPLAY_KIND)
                if value
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(is_terminal_replay_status) =>
            {
                if !ack_replay_record_has_valid_hash(&value) {
                    tracing::warn!("skipping ack replay marker with invalid content hash");
                    continue;
                }
                if let (Some(intent_id), Some(intent_content_sha256)) = (
                    value.get("intent_id").and_then(Value::as_str),
                    value.get("intent_content_sha256").and_then(Value::as_str),
                ) {
                    outstanding
                        .complete((intent_id.to_string(), intent_content_sha256.to_string()));
                }
            }
            Some(ACK_INTENT_KIND) => {
                if !ack_intent_has_supported_schema(&value) {
                    tracing::warn!("skipping ack intent with unsupported schema or retry claim");
                    continue;
                }
                if !record_has_valid_intent_hash(&value, ack_intent_hash_payload) {
                    tracing::warn!("skipping ack intent with invalid content hash");
                    continue;
                }
                if let Ok(intent) = serde_json::from_value::<QueuedAckIntent>(value) {
                    outstanding.insert(
                        (intent.intent_id.clone(), intent.content_sha256.clone()),
                        intent,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(outstanding.into_intents())
}

// ── Release-intent read-only view (for the robot status surface) ─────────────

fn release_intent_hash_payload(record: &Value) -> Value {
    json!({
        "schema_version": record["schema_version"].clone(),
        "kind": record["kind"].clone(),
        "created_ts": record["created_ts"].clone(),
        "project_key": record["project_key"].clone(),
        "agent_name": record["agent_name"].clone(),
        "paths": record["paths"].clone(),
        "file_reservation_ids": record["file_reservation_ids"].clone(),
        "failure": record["failure"].clone(),
    })
}

fn release_replay_hash_payload(record: &Value) -> Value {
    json!({
        "schema_version": record["schema_version"].clone(),
        "kind": record["kind"].clone(),
        "intent_id": record["intent_id"].clone(),
        "intent_content_sha256": record["intent_content_sha256"].clone(),
        "replayed_ts": record["replayed_ts"].clone(),
        "status": record["status"].clone(),
        "released": record["released"].clone(),
        "error_detail": record["error_detail"].clone(),
    })
}

fn release_replay_record_has_valid_hash(record: &Value) -> bool {
    let Some(content_sha256) = record.get("content_sha256").and_then(Value::as_str) else {
        return false;
    };
    let Some(intent_id) = record.get("intent_id").and_then(Value::as_str) else {
        return false;
    };
    let Some(intent_content_sha256) = record.get("intent_content_sha256").and_then(Value::as_str)
    else {
        return false;
    };
    content_sha256.len() == 64
        && intent_id.len() == 16
        && intent_content_sha256.len() == 64
        && intent_content_sha256.starts_with(intent_id)
        && content_sha256 == hash_json_value(&release_replay_hash_payload(record))
}

/// Read all outstanding release intents in first-append order.
///
/// Read-only view for the `am robot status` surface; the authoritative
/// replay-on-success path lives in [`crate::reservations`].
pub fn read_queued_release_intents(
    config: &Config,
) -> std::io::Result<Vec<QueuedReleaseIntentView>> {
    let Some(mut reader) = IntentLogReader::open(config, RELEASE_INTENT_LOG_FILE)? else {
        return Ok(Vec::new());
    };
    let mut outstanding = OutstandingIntents::new();
    while let Some(value) = reader.next_record()? {
        match value.get("kind").and_then(Value::as_str) {
            Some(RELEASE_INTENT_REPLAY_KIND)
                if value
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(is_terminal_replay_status) =>
            {
                if !release_replay_record_has_valid_hash(&value) {
                    continue;
                }
                if let (Some(intent_id), Some(intent_content_sha256)) = (
                    value.get("intent_id").and_then(Value::as_str),
                    value.get("intent_content_sha256").and_then(Value::as_str),
                ) {
                    outstanding
                        .complete((intent_id.to_string(), intent_content_sha256.to_string()));
                }
            }
            Some(RELEASE_INTENT_KIND) => {
                if !record_has_valid_intent_hash(&value, release_intent_hash_payload) {
                    continue;
                }
                if let Ok(intent) = serde_json::from_value::<QueuedReleaseIntentView>(value) {
                    outstanding.insert(
                        (intent.intent_id.clone(), intent.content_sha256.clone()),
                        intent,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(outstanding.into_intents())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(dir: &std::path::Path) -> Config {
        let mut config = Config::get();
        config.storage_root = dir.to_path_buf();
        config
    }

    #[test]
    fn append_and_read_ack_intent_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let receipt = append_ack_intent(
            &config,
            "/abs/project",
            "BlueLake",
            42,
            "acknowledge_message",
            "database disk image is malformed",
            None,
        )
        .expect("append ack intent");
        assert_eq!(receipt.intent_id.len(), 16);
        assert!(receipt.content_sha256.starts_with(&receipt.intent_id));
        assert!(receipt.intent_path.ends_with(ACK_INTENT_LOG_FILE));

        let queued = read_queued_ack_intents(&config).expect("read");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].message_id, 42);
        assert_eq!(queued[0].agent_name, "BlueLake");
        assert_eq!(queued[0].failure.stage, "acknowledge_message");
        assert_eq!(queued[0].intent_id, receipt.intent_id);
        assert_eq!(queued[0].schema_version, 1);
        assert_eq!(queued[0].idempotency, None);
        let record: Value = serde_json::from_slice(
            &std::fs::read(&receipt.intent_path).expect("original journal bytes"),
        )
        .expect("legacy record");
        assert!(record.get("idempotency").is_none());
        assert_eq!(
            receipt.content_sha256,
            hash_json_value(&json!({
                "schema_version": 1,
                "kind": ACK_INTENT_KIND,
                "created_ts": queued[0].created_ts,
                "project_key": "/abs/project",
                "agent_name": "BlueLake",
                "message_id": 42,
                "failure": {
                    "stage": "acknowledge_message",
                    "error_detail": "database disk image is malformed",
                },
            })),
            "unkeyed hashes must preserve the original on-disk contract"
        );
    }

    #[test]
    fn keyed_ack_intent_binds_claim_and_redacts_diagnostics() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let claim = AckIntentIdempotency {
            key: "private-ack-retry-token".to_string(),
            fingerprint: "a".repeat(64),
        };
        let receipt = append_ack_intent(&config, "/p", "BlueLake", 42, "ack", "busy", Some(&claim))
            .expect("keyed intent");
        let queued = read_queued_ack_intents(&config).expect("read keyed intent");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].schema_version, KEYED_ACK_INTENT_SCHEMA_VERSION);
        assert_eq!(queued[0].idempotency.as_ref(), Some(&claim));
        assert!(!format!("{:?}", queued[0]).contains(&claim.key));
        assert!(
            serde_json::to_value(&queued[0])
                .unwrap()
                .get("idempotency")
                .is_none()
        );

        let record: Value = serde_json::from_slice(
            &std::fs::read(&receipt.intent_path).expect("private journal bytes"),
        )
        .expect("journal JSON");
        assert_eq!(record["idempotency"]["key"], claim.key);
        let mut legacy_payload = ack_intent_hash_payload(&record);
        legacy_payload
            .as_object_mut()
            .unwrap()
            .remove("idempotency");
        assert_ne!(
            hash_json_value(&legacy_payload),
            receipt.content_sha256,
            "a legacy reader must reject a keyed intent rather than replay it unkeyed"
        );
        for field in ["key", "fingerprint"] {
            let mut tampered = record.clone();
            tampered["idempotency"][field] = json!("b".repeat(64));
            append_jsonl(
                &config,
                ACK_INTENT_LOG_FILE,
                ACK_INTENT_LOCK_FILE,
                &tampered,
            )
            .expect("tampered fixture");
        }
        assert_eq!(read_queued_ack_intents(&config).unwrap(), queued);
    }

    #[test]
    fn ack_intent_reader_rejects_partial_claims_downgrades_and_unknown_schemas() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let valid_claim = json!({"key": "private-retry-key", "fingerprint": "a".repeat(64)});
        for (version, claim) in [
            (1, Some(valid_claim.clone())),
            (2, None),
            (2, Some(Value::Null)),
            (2, Some(json!({"key": "partial-claim"}))),
            (2, Some(json!({"fingerprint": "a".repeat(64)}))),
            (2, Some(json!({"key": " ", "fingerprint": "a".repeat(64)}))),
            (2, Some(json!({"key": "key", "fingerprint": "invalid"}))),
            (3, Some(valid_claim)),
        ] {
            let mut record = json!({
                "schema_version": version,
                "kind": ACK_INTENT_KIND,
                "created_ts": 1,
                "project_key": "/p",
                "agent_name": "BlueLake",
                "message_id": 42,
                "failure": { "stage": "ack", "error_detail": "busy" },
            });
            if let Some(claim) = claim {
                record["idempotency"] = claim;
            }
            let hash = hash_json_value(&ack_intent_hash_payload(&record));
            record["intent_id"] = json!(&hash[..16]);
            record["content_sha256"] = json!(hash);
            append_jsonl(&config, ACK_INTENT_LOG_FILE, ACK_INTENT_LOCK_FILE, &record)
                .expect("unsupported record fixture");
        }
        assert_eq!(read_queued_ack_intents(&config).unwrap(), Vec::new());
    }

    #[test]
    fn invalid_ack_idempotency_claim_is_not_queued() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        for (key, fingerprint) in [
            (String::new(), "a".repeat(64)),
            (" key ".to_string(), "a".repeat(64)),
            (
                "k".repeat(crate::idempotency::MAX_IDEMPOTENCY_KEY_LEN + 1),
                "a".repeat(64),
            ),
            ("key".to_string(), "not-a-fingerprint".to_string()),
        ] {
            let claim = AckIntentIdempotency { key, fingerprint };
            let error =
                append_ack_intent(&config, "/p", "BlueLake", 42, "ack", "busy", Some(&claim))
                    .expect_err("invalid retry claim");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(
                error.to_string(),
                "invalid acknowledgement idempotency claim"
            );
        }
        assert!(!log_path(&config, ACK_INTENT_LOG_FILE).exists());
    }

    #[test]
    fn ack_intent_reader_isolates_torn_final_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        // Record 1: a valid queued ack intent — the log now ends in a newline.
        let receipt = append_ack_intent(
            &config,
            "/abs/project",
            "BlueLake",
            1,
            "acknowledge_message",
            "database disk image is malformed",
            None,
        )
        .expect("append intent 1");

        // Simulate a crash mid-append: a torn fragment with NO trailing newline.
        {
            use std::io::Write;
            let mut torn = std::fs::OpenOptions::new()
                .append(true)
                .open(&receipt.intent_path)
                .expect("open ack intent log");
            torn.write_all(b"{\"kind\":\"acknowledge_message_intent\",\"partial")
                .expect("write torn fragment");
        }

        // Record 2: another valid intent appended after the torn fragment.
        append_ack_intent(
            &config,
            "/abs/project",
            "BlueLake",
            2,
            "acknowledge_message",
            "database disk image is malformed",
            None,
        )
        .expect("append intent 2");

        // The leading-newline guard isolates the torn fragment on its own
        // skippable line, so the second valid record is not swallowed.
        let queued = read_queued_ack_intents(&config).expect("read");
        assert_eq!(
            queued.len(),
            2,
            "torn final line must not swallow the following valid record"
        );
    }

    #[test]
    fn replayed_ack_intent_is_filtered_out() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let receipt = append_ack_intent(
            &config,
            "/abs/project",
            "BlueLake",
            7,
            "resolve_agent",
            "pool exhausted",
            None,
        )
        .expect("append");
        assert_eq!(read_queued_ack_intents(&config).expect("read").len(), 1);

        append_ack_replay_record(
            &config,
            &receipt.intent_id,
            &receipt.content_sha256,
            "replayed",
            None,
        );
        assert_eq!(
            read_queued_ack_intents(&config).expect("read").len(),
            0,
            "replayed intent must be filtered out"
        );
    }

    #[test]
    fn failed_replay_marker_keeps_intent_outstanding() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let receipt = append_ack_intent(
            &config,
            "/p",
            "RedPeak",
            3,
            "release_reservations",
            "busy",
            None,
        )
        .expect("append");
        append_ack_replay_record(
            &config,
            &receipt.intent_id,
            &receipt.content_sha256,
            "failed",
            Some("still corrupt"),
        );
        assert_eq!(
            read_queued_ack_intents(&config).expect("read").len(),
            1,
            "a failed replay must not clear the queued intent"
        );
    }

    #[test]
    fn abandoned_replay_marker_clears_intent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let receipt = append_ack_intent(
            &config,
            "/p",
            "RedPeak",
            99,
            "acknowledge_message",
            "x",
            None,
        )
        .expect("append");
        append_ack_replay_record(
            &config,
            &receipt.intent_id,
            &receipt.content_sha256,
            REPLAY_STATUS_ABANDONED,
            Some("message no longer exists"),
        );
        assert_eq!(
            read_queued_ack_intents(&config).expect("read").len(),
            0,
            "an abandoned (permanently-unreplayable) intent must be cleared"
        );
    }

    #[test]
    fn tampered_ack_intent_hash_is_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        // Write a record with a valid-looking but wrong content_sha256.
        let bogus = json!({
            "schema_version": ACK_INTENT_SCHEMA_VERSION,
            "kind": ACK_INTENT_KIND,
            "intent_id": "0123456789abcdef",
            "content_sha256": "0123456789abcdef".to_string() + &"0".repeat(48),
            "created_ts": 1,
            "project_key": "/p",
            "agent_name": "X",
            "message_id": 1,
            "failure": { "stage": "x", "error_detail": "y" },
        });
        append_jsonl(&config, ACK_INTENT_LOG_FILE, ACK_INTENT_LOCK_FILE, &bogus).expect("append");
        assert_eq!(
            read_queued_ack_intents(&config).expect("read").len(),
            0,
            "record with mismatched content hash must be skipped"
        );
    }

    #[test]
    fn missing_log_reads_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        assert_eq!(
            read_queued_ack_intents(&config).expect("read"),
            [] as [QueuedAckIntent; 0]
        );
        assert_eq!(
            read_queued_release_intents(&config).expect("read"),
            [] as [QueuedReleaseIntentView; 0]
        );
    }

    #[test]
    fn duplicate_ack_intent_dedupes_by_content_hash() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let receipt = append_ack_intent(&config, "/p", "A", 1, "stage", "e", None).expect("append");
        let original = std::fs::read(&receipt.intent_path).unwrap();
        let record: Value = serde_json::from_slice(&original).unwrap();
        append_jsonl(&config, ACK_INTENT_LOG_FILE, ACK_INTENT_LOCK_FILE, &record).unwrap();
        let queued = read_queued_ack_intents(&config).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].content_sha256, receipt.content_sha256);
    }

    #[test]
    fn distinct_ack_intents_preserve_append_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let r1 = append_ack_intent(&config, "/p", "A", 1, "stage", "e", None).expect("append");
        let r2 = append_ack_intent(&config, "/p", "A", 2, "stage", "e", None).expect("append");
        assert_ne!(r1.intent_id, r2.intent_id);
        let queued = read_queued_ack_intents(&config).unwrap();
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].intent_id, r1.intent_id);
        assert_eq!(queued[1].intent_id, r2.intent_id);
    }

    /// Build a release-intent record byte-identical to the one
    /// `crate::reservations::append_release_intent` writes, so the read-only
    /// release view (`read_queued_release_intents`) is exercised + guarded
    /// against regression (reservations.rs keeps the authoritative writer).
    fn write_release_intent_fixture(
        config: &Config,
        created_ts: i64,
        paths: Value,
        file_reservation_ids: Value,
    ) -> (String, String) {
        let payload = json!({
            "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
            "kind": RELEASE_INTENT_KIND,
            "created_ts": created_ts,
            "project_key": "/abs/project",
            "agent_name": "BlueLake",
            "paths": paths,
            "file_reservation_ids": file_reservation_ids,
            "failure": { "stage": "release_reservations", "error_detail": "malformed" },
        });
        let content_sha256 = hash_json_value(&payload);
        let intent_id: String = content_sha256.chars().take(16).collect();
        let record = json!({
            "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
            "kind": RELEASE_INTENT_KIND,
            "intent_id": intent_id,
            "content_sha256": content_sha256,
            "created_ts": created_ts,
            "project_key": "/abs/project",
            "agent_name": "BlueLake",
            "paths": payload["paths"].clone(),
            "file_reservation_ids": payload["file_reservation_ids"].clone(),
            "failure": payload["failure"].clone(),
        });
        append_jsonl(
            config,
            RELEASE_INTENT_LOG_FILE,
            ".release_file_reservations.jsonl.lock",
            &record,
        )
        .expect("append release intent fixture");
        (intent_id, content_sha256)
    }

    const RELEASE_INTENT_SCHEMA_VERSION: u32 = 1;

    #[test]
    fn release_intent_reader_round_trip_and_replay_clear() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let (intent_id, content_sha256) =
            write_release_intent_fixture(&config, 7777, json!(["src/**"]), Value::Null);
        let queued = read_queued_release_intents(&config).expect("read release");
        assert_eq!(queued.len(), 1, "a valid release intent must be surfaced");
        assert_eq!(queued[0].agent_name, "BlueLake");
        assert_eq!(
            queued[0].paths.as_deref(),
            Some(["src/**".to_string()].as_slice())
        );
        assert_eq!(queued[0].intent_id, intent_id);

        // A terminal "replayed" marker (as reservations.rs writes) clears it.
        let replay_payload = json!({
            "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
            "kind": RELEASE_INTENT_REPLAY_KIND,
            "intent_id": intent_id,
            "intent_content_sha256": content_sha256,
            "replayed_ts": 8888,
            "status": "replayed",
            "released": 1,
            "error_detail": Value::Null,
        });
        let replay_hash = hash_json_value(&replay_payload);
        let replay_record = json!({
            "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
            "kind": RELEASE_INTENT_REPLAY_KIND,
            "intent_id": intent_id,
            "content_sha256": replay_hash,
            "intent_content_sha256": content_sha256,
            "replayed_ts": 8888,
            "status": "replayed",
            "released": 1,
            "error_detail": Value::Null,
        });
        append_jsonl(
            &config,
            RELEASE_INTENT_LOG_FILE,
            ".release_file_reservations.jsonl.lock",
            &replay_record,
        )
        .expect("append replay marker");
        assert!(
            read_queued_release_intents(&config)
                .expect("read after replay")
                .is_empty(),
            "a replayed release intent must be filtered out"
        );
    }

    #[test]
    fn release_intent_reader_clears_abandoned_intent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let (intent_id, content_sha256) =
            write_release_intent_fixture(&config, 7777, json!(["src/**"]), Value::Null);
        assert_eq!(
            read_queued_release_intents(&config)
                .expect("read release")
                .len(),
            1
        );

        // A terminal "abandoned" marker (written by reservations.rs when the
        // queued release is permanently un-replayable — e.g. the agent/project no
        // longer exists) must also clear the intent from the robot-status view,
        // not just "replayed". Before the fix this read-only view only treated
        // "replayed" as terminal, so an abandoned intent was surfaced forever with
        // a replay action that could never succeed.
        let replay_payload = json!({
            "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
            "kind": RELEASE_INTENT_REPLAY_KIND,
            "intent_id": intent_id,
            "intent_content_sha256": content_sha256,
            "replayed_ts": 8888,
            "status": "abandoned",
            "released": 0,
            "error_detail": "agent no longer exists",
        });
        let replay_hash = hash_json_value(&replay_payload);
        let replay_record = json!({
            "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
            "kind": RELEASE_INTENT_REPLAY_KIND,
            "intent_id": intent_id,
            "content_sha256": replay_hash,
            "intent_content_sha256": content_sha256,
            "replayed_ts": 8888,
            "status": "abandoned",
            "released": 0,
            "error_detail": "agent no longer exists",
        });
        append_jsonl(
            &config,
            RELEASE_INTENT_LOG_FILE,
            ".release_file_reservations.jsonl.lock",
            &replay_record,
        )
        .expect("append abandoned marker");
        assert!(
            read_queued_release_intents(&config)
                .expect("read after abandon")
                .is_empty(),
            "an abandoned release intent must be filtered out of the robot-status view"
        );
    }

    #[test]
    fn release_intent_reader_skips_tampered_hash() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let bogus = json!({
            "schema_version": RELEASE_INTENT_SCHEMA_VERSION,
            "kind": RELEASE_INTENT_KIND,
            "intent_id": "0123456789abcdef",
            "content_sha256": "0123456789abcdef".to_string() + &"0".repeat(48),
            "created_ts": 1,
            "project_key": "/p",
            "agent_name": "X",
            "paths": Value::Null,
            "file_reservation_ids": json!([42]),
            "failure": { "stage": "s", "error_detail": "e" },
        });
        append_jsonl(
            &config,
            RELEASE_INTENT_LOG_FILE,
            ".release_file_reservations.jsonl.lock",
            &bogus,
        )
        .expect("append");
        assert!(
            read_queued_release_intents(&config)
                .expect("read")
                .is_empty(),
            "a release record with a mismatched content hash must be skipped"
        );
    }

    fn append_torn_unicode(path: &Path) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(b"{\"failure\":\"interrupted \xe2\x82")
            .unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn torn_utf8_does_not_disable_ack_recovery_or_terminal_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let first = append_ack_intent(&config, "/p", "BlueLake", 1, "ack", "busy", None).unwrap();
        append_torn_unicode(&first.intent_path);
        assert!(std::fs::read_to_string(&first.intent_path).is_err());
        append_ack_intent(&config, "/p", "BlueLake", 2, "ack", "busy", None).unwrap();
        let queued = read_queued_ack_intents(&config).unwrap();
        assert_eq!(
            queued
                .iter()
                .map(|intent| intent.message_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        append_ack_replay_record(
            &config,
            &first.intent_id,
            &first.content_sha256,
            REPLAY_STATUS_REPLAYED,
            None,
        );
        let queued = read_queued_ack_intents(&config).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].message_id, 2);
    }

    #[test]
    fn torn_utf8_does_not_hide_release_status() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        write_release_intent_fixture(&config, 1, Value::Null, json!([1]));
        append_torn_unicode(&log_path(&config, RELEASE_INTENT_LOG_FILE));
        write_release_intent_fixture(&config, 2, Value::Null, json!([2]));
        let queued = read_queued_release_intents(&config).unwrap();
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].created_ts, 1);
        assert_eq!(queued[1].created_ts, 2);
    }

    #[test]
    fn reader_does_not_chase_records_appended_after_open() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        append_jsonl(
            &config,
            ACK_INTENT_LOG_FILE,
            ACK_INTENT_LOCK_FILE,
            &json!({"n": 1}),
        )
        .unwrap();
        let mut reader = IntentLogReader::open(&config, ACK_INTENT_LOG_FILE)
            .unwrap()
            .unwrap();
        append_jsonl(
            &config,
            ACK_INTENT_LOG_FILE,
            ACK_INTENT_LOCK_FILE,
            &json!({"n": 2}),
        )
        .unwrap();
        assert_eq!(reader.next_record().unwrap(), Some(json!({"n": 1})));
        assert!(reader.next_record().unwrap().is_none());
        assert!(reader.next_record().unwrap().is_none());
        let mut fresh = IntentLogReader::open(&config, ACK_INTENT_LOG_FILE)
            .unwrap()
            .unwrap();
        assert_eq!(fresh.next_record().unwrap(), Some(json!({"n": 1})));
        assert_eq!(fresh.next_record().unwrap(), Some(json!({"n": 2})));
        assert!(fresh.next_record().unwrap().is_none());
    }

    #[test]
    fn reader_bounds_delimiter_free_records_before_allocating_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let path = log_path(&config, ACK_INTENT_LOG_FILE);
        ensure_intent_parent(&path).unwrap();
        std::fs::write(&path, b"{\"long\":\"abcdefghijklmnopqrstuvwxyz\"}").unwrap();
        let mut reader = IntentLogReader::open(&config, ACK_INTENT_LOG_FILE)
            .unwrap()
            .unwrap();
        reader.record_limit = 16;
        assert_eq!(
            reader.next_record().unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(reader.line.len(), 17);
        assert!(std::fs::metadata(&path).unwrap().len() > 17);
    }

    #[test]
    fn reader_accepts_exact_record_bound_and_complete_unterminated_json() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let path = log_path(&config, ACK_INTENT_LOG_FILE);
        ensure_intent_parent(&path).unwrap();
        for bytes in [b"{\"n\":1}\n".as_slice(), b"{\"n\":1}".as_slice()] {
            std::fs::write(&path, bytes).unwrap();
            let mut reader = IntentLogReader::open(&config, ACK_INTENT_LOG_FILE)
                .unwrap()
                .unwrap();
            reader.record_limit = bytes.len();
            assert_eq!(reader.next_record().unwrap(), Some(json!({"n": 1})));
            assert!(reader.next_record().unwrap().is_none());
        }
    }

    #[test]
    fn truncated_snapshot_is_not_a_successful_empty_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let path = append_jsonl(
            &config,
            ACK_INTENT_LOG_FILE,
            ACK_INTENT_LOCK_FILE,
            &json!({"n": 1}),
        )
        .unwrap();
        let mut reader = IntentLogReader::open(&config, ACK_INTENT_LOG_FILE)
            .unwrap()
            .unwrap();
        let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        writer.set_len(0).unwrap();
        assert_eq!(
            reader.next_record().unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn nonregular_intent_log_is_an_error_not_an_empty_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        std::fs::create_dir_all(log_path(&config, ACK_INTENT_LOG_FILE)).unwrap();
        assert!(read_queued_ack_intents(&config).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn intent_readers_refuse_symlinked_parent_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join(DEGRADED_INTENTS_DIR)).unwrap();
        assert!(read_queued_ack_intents(&config).is_err());
        assert!(read_queued_release_intents(&config).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn duplicate_release_records_are_reported_once() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let (id, hash) = write_release_intent_fixture(&config, 1, Value::Null, json!([42]));
        write_release_intent_fixture(&config, 1, Value::Null, json!([42]));
        let queued = read_queued_release_intents(&config).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].intent_id, id);
        assert_eq!(queued[0].content_sha256, hash);
    }

    fn open_test_lock(config: &Config) -> std::fs::File {
        let path = lock_path(config, ACK_INTENT_LOCK_FILE);
        ensure_intent_parent(&path).unwrap();
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
    }

    #[test]
    fn lock_error_recognizes_platform_contention_only() {
        assert!(is_lock_contention(&fs2::lock_contended_error()));
        assert!(is_lock_contention(&std::io::Error::from(
            std::io::ErrorKind::WouldBlock
        )));
        assert!(!is_lock_contention(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
        assert!(!is_lock_contention(&std::io::Error::other(
            "not contention"
        )));
    }

    #[test]
    fn uncontended_lock_succeeds_with_zero_wait_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let lock = open_test_lock(&config);
        acquire_intent_lock(&lock, Duration::ZERO).unwrap();
        let contender = open_test_lock(&config);
        assert_eq!(
            acquire_intent_lock(&contender, Duration::ZERO)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        drop(lock);
        acquire_intent_lock(&contender, Duration::ZERO).unwrap();
    }

    #[test]
    fn contended_append_times_out_without_record_and_retry_recovers() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let first = append_ack_intent(&config, "/p", "BlueLake", 1, "ack", "busy", None).unwrap();
        let before = std::fs::read(&first.intent_path).unwrap();
        let held = open_test_lock(&config);
        fs2::FileExt::lock_exclusive(&held).unwrap();
        let started = Instant::now();
        let error =
            append_ack_intent(&config, "/p", "BlueLake", 2, "ack", "busy", None).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(std::fs::read(&first.intent_path).unwrap(), before);
        drop(held);
        append_ack_intent(&config, "/p", "BlueLake", 2, "ack", "busy", None).unwrap();
        let queued = read_queued_ack_intents(&config).unwrap();
        assert_eq!(
            queued
                .iter()
                .map(|intent| intent.message_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn replay_marker_contention_does_not_clear_pending_intent() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let receipt = append_ack_intent(&config, "/p", "BlueLake", 1, "ack", "busy", None).unwrap();
        let before = std::fs::read(&receipt.intent_path).unwrap();
        let held = open_test_lock(&config);
        fs2::FileExt::lock_exclusive(&held).unwrap();
        let started = Instant::now();
        append_ack_replay_record(
            &config,
            &receipt.intent_id,
            &receipt.content_sha256,
            REPLAY_STATUS_REPLAYED,
            None,
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(std::fs::read(&receipt.intent_path).unwrap(), before);
        assert_eq!(read_queued_ack_intents(&config).unwrap().len(), 1);
        drop(held);
        append_ack_replay_record(
            &config,
            &receipt.intent_id,
            &receipt.content_sha256,
            REPLAY_STATUS_REPLAYED,
            None,
        );
        assert_eq!(read_queued_ack_intents(&config).unwrap(), []);
    }

    #[test]
    fn distinct_journal_locks_do_not_block_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let held = open_test_lock(&config);
        fs2::FileExt::lock_exclusive(&held).unwrap();
        write_release_intent_fixture(&config, 1, Value::Null, json!([42]));
        assert_eq!(read_queued_release_intents(&config).unwrap().len(), 1);
        assert!(!log_path(&config, ACK_INTENT_LOG_FILE).exists());
    }

    #[test]
    fn oversized_append_is_refused_before_creating_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let record = Value::String("x".repeat(MAX_INTENT_RECORD_BYTES));
        let error =
            append_jsonl(&config, ACK_INTENT_LOG_FILE, ACK_INTENT_LOCK_FILE, &record).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!tmp.path().join(DEGRADED_INTENTS_DIR).exists());
    }

    #[test]
    fn streaming_accumulator_releases_completed_payloads_immediately() {
        use std::sync::Arc;

        let payload = Arc::new(vec![0_u8; 4096]);
        let mut outstanding = OutstandingIntents::new();
        for index in 0..10_000 {
            let key = ("prefix".to_string(), index.to_string());
            outstanding
                .insert(key.clone(), Arc::clone(&payload))
                .unwrap();
            assert_eq!(Arc::strong_count(&payload), 2);
            outstanding.complete(key);
            assert_eq!(Arc::strong_count(&payload), 1);
            assert!(outstanding.pending.is_empty());
        }
        assert_eq!(outstanding.terminal.len(), 10_000);
        assert_eq!(outstanding.into_intents(), []);
    }

    #[test]
    fn streaming_accumulator_preserves_original_order_and_first_payload() {
        let a = ("prefix".into(), "a".into());
        let b = ("prefix".into(), "b".into());
        let c = ("prefix".into(), "c".into());
        let mut outstanding = OutstandingIntents::new();
        outstanding.insert(c.clone(), 3).unwrap();
        outstanding.insert(a.clone(), 1).unwrap();
        outstanding.insert(c, 999).unwrap();
        outstanding.complete(a);
        outstanding.insert(b, 2).unwrap();
        assert_eq!(outstanding.into_intents(), vec![3, 2]);
    }

    #[test]
    fn streaming_accumulator_terminal_markers_cannot_be_undone() {
        let key = ("prefix".into(), "hash".into());
        let mut outstanding = OutstandingIntents::new();
        outstanding.complete(key.clone());
        outstanding.insert(key.clone(), "late original").unwrap();
        outstanding.complete(key.clone());
        outstanding.insert(key, "duplicate").unwrap();
        assert_eq!(outstanding.into_intents(), [] as [&str; 0]);
    }

    #[test]
    fn streaming_accumulator_keeps_full_hash_identity() {
        let first = ("same-prefix".into(), "full-hash-one".into());
        let second = ("same-prefix".into(), "full-hash-two".into());
        let mut outstanding = OutstandingIntents::new();
        outstanding.insert(first.clone(), 1).unwrap();
        outstanding.insert(second, 2).unwrap();
        outstanding.complete(first);
        assert_eq!(outstanding.into_intents(), vec![2]);
    }

    #[test]
    fn streaming_accumulator_overflow_fails_without_reordering() {
        let mut outstanding = OutstandingIntents::new();
        outstanding.next_order = u64::MAX - 1;
        outstanding
            .insert(("prefix".into(), "first".into()), 1)
            .unwrap();
        let error = outstanding
            .insert(("prefix".into(), "second".into()), 2)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(outstanding.into_intents(), vec![1]);
    }

    #[test]
    fn ack_replay_marker_before_intent_still_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let receipt = append_ack_intent(&config, "/p", "BlueLake", 1, "ack", "busy", None).unwrap();
        append_ack_replay_record(
            &config,
            &receipt.intent_id,
            &receipt.content_sha256,
            REPLAY_STATUS_REPLAYED,
            None,
        );
        let records = std::fs::read_to_string(&receipt.intent_path).unwrap();
        let lines: Vec<_> = records.lines().collect();
        assert_eq!(lines.len(), 2);
        // Model imported/duplicated journal history, not a new acknowledgment.
        std::fs::write(
            &receipt.intent_path,
            format!("{}\n{}\n{}\n", lines[1], lines[0], lines[0]),
        )
        .unwrap();
        append_ack_replay_record(
            &config,
            &receipt.intent_id,
            &receipt.content_sha256,
            REPLAY_STATUS_FAILED,
            Some("stale failed retry"),
        );
        assert_eq!(read_queued_ack_intents(&config).unwrap(), []);
    }

    #[test]
    fn valid_marker_for_different_full_hash_does_not_clear_intent() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let receipt = append_ack_intent(&config, "/p", "BlueLake", 1, "ack", "busy", None).unwrap();
        let mut other_hash = receipt.content_sha256.clone();
        let replacement = if other_hash.ends_with('0') { "1" } else { "0" };
        other_hash.replace_range(63..64, replacement);
        assert!(other_hash.starts_with(&receipt.intent_id));
        append_ack_replay_record(
            &config,
            &receipt.intent_id,
            &other_hash,
            REPLAY_STATUS_REPLAYED,
            None,
        );
        let queued = read_queued_ack_intents(&config).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].content_sha256, receipt.content_sha256);
    }

    #[test]
    fn malformed_terminal_marker_cannot_discard_pending_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let receipt = append_ack_intent(&config, "/p", "BlueLake", 1, "ack", "busy", None).unwrap();
        append_jsonl(
            &config,
            ACK_INTENT_LOG_FILE,
            ACK_INTENT_LOCK_FILE,
            &json!({
                "kind": ACK_INTENT_REPLAY_KIND,
                "schema_version": ACK_INTENT_SCHEMA_VERSION,
                "intent_id": receipt.intent_id,
                "intent_content_sha256": receipt.content_sha256,
                "content_sha256": "0".repeat(64),
                "replayed_ts": 1,
                "status": REPLAY_STATUS_REPLAYED,
                "error_detail": null,
            }),
        )
        .unwrap();
        assert_eq!(read_queued_ack_intents(&config).unwrap().len(), 1);
    }

    #[test]
    fn hard_linked_ack_log_is_refused_without_changing_its_source() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let path = log_path(&config, ACK_INTENT_LOG_FILE);
        ensure_intent_parent(&path).unwrap();
        let source = tmp.path().join("source-evidence");
        std::fs::write(&source, b"preserved evidence").unwrap();
        std::fs::hard_link(&source, &path).unwrap();
        assert!(append_ack_intent(&config, "/p", "BlueLake", 1, "ack", "busy", None).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"preserved evidence");
        assert!(read_queued_ack_intents(&config).is_err());
    }

    #[test]
    fn hard_linked_ack_lock_is_refused_before_the_log_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let path = lock_path(&config, ACK_INTENT_LOCK_FILE);
        ensure_intent_parent(&path).unwrap();
        let source = tmp.path().join("source-evidence");
        std::fs::write(&source, b"preserved lock evidence").unwrap();
        std::fs::hard_link(&source, &path).unwrap();
        assert!(append_ack_intent(&config, "/p", "BlueLake", 1, "ack", "busy", None).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"preserved lock evidence");
        assert!(!log_path(&config, ACK_INTENT_LOG_FILE).exists());
    }

    #[test]
    fn invalid_or_aliased_entry_names_do_not_create_journal_state() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        for (log, lock) in [
            ("../outside", ACK_INTENT_LOCK_FILE),
            (ACK_INTENT_LOG_FILE, "../outside"),
            ("ack:stream", ACK_INTENT_LOCK_FILE),
            ("NUL", ACK_INTENT_LOCK_FILE),
            ("same", "SAME"),
        ] {
            assert!(append_jsonl(&config, log, lock, &json!({"n": 1})).is_err());
            assert!(!tmp.path().join(DEGRADED_INTENTS_DIR).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_lock_or_log_cannot_issue_an_ack_receipt() {
        for name in [ACK_INTENT_LOG_FILE, ACK_INTENT_LOCK_FILE] {
            let tmp = tempfile::tempdir().unwrap();
            let config = test_config(tmp.path());
            let path = log_path(&config, name);
            ensure_intent_parent(&path).unwrap();
            let source = tmp.path().join("source-evidence");
            std::fs::write(&source, b"unchanged").unwrap();
            std::os::unix::fs::symlink(&source, &path).unwrap();
            assert!(append_ack_intent(&config, "/p", "BlueLake", 1, "ack", "busy", None).is_err());
            assert_eq!(std::fs::read(&source).unwrap(), b"unchanged");
        }
    }

    #[cfg(unix)]
    #[test]
    fn replaced_log_is_an_error_at_snapshot_completion() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let path = append_jsonl(
            &config,
            ACK_INTENT_LOG_FILE,
            ACK_INTENT_LOCK_FILE,
            &json!({"n": 1}),
        )
        .unwrap();
        let mut reader = IntentLogReader::open(&config, ACK_INTENT_LOG_FILE)
            .unwrap()
            .unwrap();
        assert_eq!(reader.next_record().unwrap(), Some(json!({"n": 1})));
        std::fs::rename(&path, path.with_extension("preserved")).unwrap();
        std::fs::write(&path, b"{\"n\":2}\n").unwrap();
        let error = reader.next_record().unwrap_err();
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!reader.finished);
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"n\":2}\n");
    }

    #[cfg(unix)]
    #[test]
    fn displaced_directory_cannot_finish_as_a_successful_empty_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        append_jsonl(
            &config,
            ACK_INTENT_LOG_FILE,
            ACK_INTENT_LOCK_FILE,
            &json!({"n": 1}),
        )
        .unwrap();
        let mut reader = IntentLogReader::open(&config, ACK_INTENT_LOG_FILE)
            .unwrap()
            .unwrap();
        std::fs::rename(
            tmp.path().join(DEGRADED_INTENTS_DIR),
            tmp.path().join("preserved-journal"),
        )
        .unwrap();
        assert_eq!(reader.next_record().unwrap(), Some(json!({"n": 1})));
        let error = reader.next_record().unwrap_err();
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!reader.finished);
        assert!(
            tmp.path()
                .join("preserved-journal")
                .join(ACK_INTENT_LOG_FILE)
                .exists()
        );
    }

    proptest::proptest! {
        #[test]
        fn streaming_accumulator_matches_reference(
            history in proptest::collection::vec((0_u8..4, 0_u8..16), 0..256)
        ) {
            let terminal: HashSet<_> = history
                .iter()
                .filter_map(|(kind, id)| matches!(*kind, 1 | 2).then_some(*id))
                .collect();
            let mut seen = HashSet::new();
            let expected: Vec<_> = history
                .iter()
                .filter_map(|(kind, id)| {
                    (*kind == 0 && seen.insert(*id) && !terminal.contains(id)).then_some(*id)
                })
                .collect();
            let mut outstanding = OutstandingIntents::new();
            for (kind, id) in history {
                let key = ("shared-prefix".to_string(), id.to_string());
                match kind {
                    0 => outstanding.insert(key, id).unwrap(),
                    1 | 2 => outstanding.complete(key),
                    _ => {} // Failed markers never clear or resurrect work.
                }
            }
            proptest::prop_assert_eq!(outstanding.into_intents(), expected);
        }
    }
}
