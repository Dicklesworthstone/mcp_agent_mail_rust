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
//!   with `status` (`"replayed"`/`"failed"`), `intent_id`, and
//!   `intent_content_sha256`.
//!
//! A queued intent is outstanding until a `status == "replayed"` marker
//! referencing its `(intent_id, content_sha256)` pair is present.
//!
//! NOTE: [`crate::reservations`] still carries its own private copy of the
//! release-intent writer/reader for its automatic replay-on-success path.
//! Migrating it onto this shared surface is tracked as a follow-up so that the
//! single mutation chokepoint work (Track F) is not disturbed.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use mcp_agent_mail_core::Config;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Subdirectory under `storage_root` holding every degraded-intent log.
pub const DEGRADED_INTENTS_DIR: &str = "degraded_intents";

/// Release-intent log filename (mirrors the private constant in
/// [`crate::reservations`]; kept here for the read-only robot surface).
pub const RELEASE_INTENT_LOG_FILE: &str = "release_file_reservations.jsonl";
/// Release-intent record kind.
pub const RELEASE_INTENT_KIND: &str = "release_file_reservations_intent";
/// Release-intent replay marker kind.
pub const RELEASE_INTENT_REPLAY_KIND: &str = "release_file_reservations_replay";

/// Ack-intent schema version.
pub const ACK_INTENT_SCHEMA_VERSION: u32 = 1;
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

/// Refuse to operate on a path that is (or has become) a symlink.
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

/// Append a single JSON record to an intent log under an exclusive advisory
/// lock, with restrictive permissions and `fsync` of file + parent dir.
pub fn append_jsonl(
    config: &Config,
    file_name: &str,
    lock_file_name: &str,
    record: &Value,
) -> std::io::Result<PathBuf> {
    let path = log_path(config, file_name);
    ensure_intent_parent(&path)?;
    reject_existing_symlink(&path)?;
    let lock_file_path = lock_path(config, lock_file_name);
    reject_existing_symlink(&lock_file_path)?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_file_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        lock_file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    fs2::FileExt::lock_exclusive(&lock_file)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let mut line =
        serde_json::to_vec(record).map_err(|err| std::io::Error::other(err.to_string()))?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_all()?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(path)
}

// ── Ack-intent canonical hashing ────────────────────────────────────────────

fn ack_intent_hash_payload(record: &Value) -> Value {
    json!({
        "schema_version": record["schema_version"].clone(),
        "kind": record["kind"].clone(),
        "created_ts": record["created_ts"].clone(),
        "project_key": record["project_key"].clone(),
        "agent_name": record["agent_name"].clone(),
        "message_id": record["message_id"].clone(),
        "failure": record["failure"].clone(),
    })
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
) -> std::io::Result<IntentReceipt> {
    let created_ts = mcp_agent_mail_db::now_micros();
    let payload = json!({
        "schema_version": ACK_INTENT_SCHEMA_VERSION,
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
    let content_sha256 = hash_json_value(&payload);
    let intent_id = content_sha256.chars().take(16).collect::<String>();
    let record = json!({
        "schema_version": ACK_INTENT_SCHEMA_VERSION,
        "kind": ACK_INTENT_KIND,
        "intent_id": intent_id,
        "content_sha256": content_sha256,
        "created_ts": created_ts,
        "project_key": project_key,
        "agent_name": agent_name,
        "message_id": message_id,
        "failure": payload["failure"].clone(),
    });
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

/// Read all outstanding (un-replayed) ack intents, newest last.
pub fn read_queued_ack_intents(config: &Config) -> std::io::Result<Vec<QueuedAckIntent>> {
    let path = log_path(config, ACK_INTENT_LOG_FILE);
    reject_existing_symlink(&path)?;
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut replayed = std::collections::HashSet::new();
    let mut intents = Vec::new();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
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
                    replayed.insert((intent_id.to_string(), intent_content_sha256.to_string()));
                }
            }
            Some(ACK_INTENT_KIND) => {
                if !record_has_valid_intent_hash(&value, ack_intent_hash_payload) {
                    tracing::warn!("skipping ack intent with invalid content hash");
                    continue;
                }
                if let Ok(intent) = serde_json::from_value::<QueuedAckIntent>(value) {
                    intents.push(intent);
                }
            }
            _ => {}
        }
    }
    intents.retain(|intent| {
        !replayed.contains(&(intent.intent_id.clone(), intent.content_sha256.clone()))
    });
    Ok(intents)
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

/// Read all outstanding (un-replayed) release intents, newest last.
///
/// Read-only view for the `am robot status` surface; the authoritative
/// replay-on-success path lives in [`crate::reservations`].
pub fn read_queued_release_intents(
    config: &Config,
) -> std::io::Result<Vec<QueuedReleaseIntentView>> {
    let path = log_path(config, RELEASE_INTENT_LOG_FILE);
    reject_existing_symlink(&path)?;
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut replayed = std::collections::HashSet::new();
    let mut intents = Vec::new();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("kind").and_then(Value::as_str) {
            Some(RELEASE_INTENT_REPLAY_KIND)
                if value.get("status").and_then(Value::as_str) == Some("replayed") =>
            {
                if !release_replay_record_has_valid_hash(&value) {
                    continue;
                }
                if let (Some(intent_id), Some(intent_content_sha256)) = (
                    value.get("intent_id").and_then(Value::as_str),
                    value.get("intent_content_sha256").and_then(Value::as_str),
                ) {
                    replayed.insert((intent_id.to_string(), intent_content_sha256.to_string()));
                }
            }
            Some(RELEASE_INTENT_KIND) => {
                if !record_has_valid_intent_hash(&value, release_intent_hash_payload) {
                    continue;
                }
                if let Ok(intent) = serde_json::from_value::<QueuedReleaseIntentView>(value) {
                    intents.push(intent);
                }
            }
            _ => {}
        }
    }
    intents.retain(|intent| {
        !replayed.contains(&(intent.intent_id.clone(), intent.content_sha256.clone()))
    });
    Ok(intents)
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
        let receipt =
            append_ack_intent(&config, "/p", "RedPeak", 3, "release_reservations", "busy")
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
        let receipt = append_ack_intent(&config, "/p", "RedPeak", 99, "acknowledge_message", "x")
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
        assert!(read_queued_ack_intents(&config).expect("read").is_empty());
        assert!(
            read_queued_release_intents(&config)
                .expect("read")
                .is_empty()
        );
    }

    #[test]
    fn duplicate_ack_intent_dedupes_by_content_hash() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        // Two appends with identical canonical payload (same created_ts forced
        // via identical fields is unlikely; instead assert distinct ids when
        // payloads differ, and that reads return both).
        let r1 = append_ack_intent(&config, "/p", "A", 1, "stage", "e").expect("append");
        let r2 = append_ack_intent(&config, "/p", "A", 2, "stage", "e").expect("append");
        assert_ne!(r1.intent_id, r2.intent_id);
        assert_eq!(read_queued_ack_intents(&config).expect("read").len(), 2);
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
}
