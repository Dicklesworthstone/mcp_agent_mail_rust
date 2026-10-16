//! Recover a WBQ durability refusal only after its exact failed mail is verified.
//!
//! The failure path retains bounded identifiers and content digests, not Config,
//! bearer tokens or replayable message payloads. Repair reads the live source and
//! uses the existing no-clobber, BCC-safe, Git-verifying reconciler. Unknown,
//! oversized and non-message failures require the existing operator clear.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, UNIX_EPOCH};

use asupersync::Cx;
use fastmcp_core::block_on;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbPool, corruption_circuit_breaker};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    MAX_DB_PAYLOAD_BYTES, MAX_RETRIES_PER_PASS, MAX_RETRY_IDS, PreparedMessage, ReconcileCursor,
    outcome, prepare_message_classified, source_error, validate_pool_binding,
};
use crate::WriteOp;

const MAX_FAILURES: usize = 1024;
const RETRY_DELAY: Duration = Duration::from_secs(60);
const MAX_SCOPE_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Scope {
    database: PathBuf,
    archive: PathBuf,
    file_identity: String,
}

impl Scope {
    fn capture(config: &Config) -> Option<Self> {
        let database =
            mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url)?;
        let database = std::fs::canonicalize(database).ok()?;
        let archive = std::fs::canonicalize(&config.storage_root).ok()?;
        if database.as_os_str().as_encoded_bytes().len()
            + archive.as_os_str().as_encoded_bytes().len()
            > MAX_SCOPE_BYTES
        {
            return None;
        }
        let metadata = std::fs::metadata(&database).ok()?;
        if !metadata.is_file() || !archive.is_dir() {
            return None;
        }
        let created = metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|time| (time.as_secs(), time.subsec_nanos()));
        #[cfg(unix)]
        let file_identity = {
            use std::os::unix::fs::MetadataExt;
            format!("{}:{}:{created:?}", metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let file_identity = format!("{:?}", created?);
        Some(Self {
            database,
            archive,
            file_identity,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Witness {
    scope: Scope,
    project: String,
    id: i64,
    digest: [u8; 32],
}

/// A streaming, bounded canonical JSON digest. Object insertion order is not
/// message content, even when another workspace dependency enables preserve_order.
/// Array ordering and every field/value remain significant; nothing is redacted
/// from the digest or logged. Length/type framing prevents ambiguous concatenation.
struct Fingerprint {
    hash: Sha256,
    remaining: usize,
}

impl Fingerprint {
    fn feed(&mut self, bytes: &[u8]) -> Option<()> {
        self.remaining = self.remaining.checked_sub(bytes.len())?;
        self.hash.update(bytes);
        Some(())
    }

    fn text(&mut self, value: &str) -> Option<()> {
        self.feed(&u64::try_from(value.len()).ok()?.to_le_bytes())?;
        self.feed(value.as_bytes())
    }

    fn json(&mut self, value: &Value, depth: usize) -> Option<()> {
        if depth > 64 {
            return None;
        }
        match value {
            Value::Null => self.feed(b"n"),
            Value::Bool(value) => self.feed(if *value { b"t" } else { b"f" }),
            Value::Number(value) => {
                self.feed(b"d")?;
                self.text(&value.to_string())
            }
            Value::String(value) => {
                self.feed(b"s")?;
                self.text(value)
            }
            Value::Array(values) => {
                self.feed(b"a")?;
                self.feed(&u64::try_from(values.len()).ok()?.to_le_bytes())?;
                for value in values {
                    self.json(value, depth + 1)?;
                }
                Some(())
            }
            Value::Object(values) => {
                self.feed(b"o")?;
                self.feed(&u64::try_from(values.len()).ok()?.to_le_bytes())?;
                // Each key consumes at least eight length bytes. Reject an
                // oversized map before allocating its sorted reference list.
                if values.len() > self.remaining / 8 {
                    return None;
                }
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                for key in keys {
                    self.text(key)?;
                    self.json(&values[key], depth + 1)?;
                }
                Some(())
            }
        }
    }
}

fn fingerprint(
    message: &Value,
    body: &str,
    sender: &str,
    recipients: &[String],
) -> Option<[u8; 32]> {
    if recipients.len() > 1024 || body.len() > usize::try_from(MAX_DB_PAYLOAD_BYTES).ok()? {
        return None;
    }
    let mut fingerprint = Fingerprint {
        hash: Sha256::new(),
        remaining: usize::try_from(MAX_DB_PAYLOAD_BYTES).ok()?,
    };
    fingerprint.feed(b"agent-mail-wbq-recovery-v1")?;
    fingerprint.json(message, 0)?;
    fingerprint.text(body)?;
    fingerprint.text(sender)?;
    // Archive materialization already deduplicates inbox destinations. Keep
    // full TO/CC/BCC order in the frontmatter above, but normalize this path set.
    let mut recipients = recipients.iter().map(String::as_str).collect::<Vec<_>>();
    recipients.sort_unstable();
    recipients.dedup();
    fingerprint.feed(&u64::try_from(recipients.len()).ok()?.to_le_bytes())?;
    for recipient in recipients {
        fingerprint.text(recipient)?;
    }
    Some(fingerprint.hash.finalize().into())
}

impl Witness {
    fn from_op(op: &WriteOp) -> Option<Self> {
        let WriteOp::MessageBundle {
            config,
            project_slug,
            message_json,
            body_md,
            sender,
            recipients,
            extra_paths,
        } = op
        else {
            return None;
        };
        // The normal reconciler cannot authorize arbitrary extra commit paths.
        // Preserve operator refusal instead of pretending they were repaired.
        if !extra_paths.is_empty() || project_slug.len() > 1024 {
            return None;
        }
        let id = message_json.get("id")?.as_i64().filter(|id| *id > 0)?;
        if message_json.get("project_slug")?.as_str()? != project_slug.as_str()
            || message_json.get("from")?.as_str()? != sender.as_str()
        {
            return None;
        }
        Some(Self {
            scope: Scope::capture(config)?,
            project: project_slug.clone(),
            id,
            digest: fingerprint(message_json, body_md, sender, recipients)?,
        })
    }

    fn matches(&self, prepared: &PreparedMessage) -> bool {
        prepared.project_slug == self.project
            && prepared.message.get("id").and_then(Value::as_i64) == Some(self.id)
            && fingerprint(
                &prepared.message,
                &prepared.body,
                &prepared.sender,
                &prepared.recipients,
            ) == Some(self.digest)
    }
}

#[derive(Clone, Debug)]
pub(super) struct Ticket {
    sequence: u64,
    witness: Witness,
    ready_at: Instant,
}

#[derive(Default)]
struct Ledger {
    next_sequence: u64,
    timestamp: u64,
    unknown_failure: bool,
    pending_registrations: usize,
    entries: VecDeque<Ticket>,
}

impl Ledger {
    fn record(&mut self, witness: Option<Witness>, now: Instant) {
        let Some(witness) = witness else {
            self.unknown_failure = true;
            return;
        };
        let Some(sequence) = self.next_sequence.checked_add(1) else {
            self.unknown_failure = true;
            return;
        };
        self.next_sequence = sequence;
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.witness == witness)
        {
            self.entries.remove(index);
        } else if self.entries.len() == MAX_FAILURES {
            self.unknown_failure = true;
            return;
        }
        self.entries.push_back(Ticket {
            sequence,
            witness,
            ready_at: now,
        });
    }

    fn complete(&mut self, ticket: &Ticket) -> bool {
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.sequence == ticket.sequence && entry.witness == ticket.witness)
        else {
            return false;
        };
        self.entries.remove(index);
        self.entries.is_empty() && !self.unknown_failure && self.pending_registrations == 0
    }

    fn postpone(&mut self, ticket: &Ticket, now: Instant) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.sequence == ticket.sequence)
            && let Some(mut entry) = self.entries.remove(index)
        {
            entry.ready_at = now + RETRY_DELAY;
            self.entries.push_back(entry);
        }
    }

    fn operator_clear(&mut self) {
        self.entries.clear();
        self.unknown_failure = false;
        self.timestamp = 0;
        // Do not reset sequence: an old in-flight proof cannot discharge a new
        // failure recorded after the operator clear, even for identical mail.
        // Nor may an operator clear erase a registration still collecting its
        // witness. That registration must finish (or fail closed) independently.
    }
}

static LEDGER: LazyLock<Mutex<Ledger>> = LazyLock::new(|| Mutex::new(Ledger::default()));

fn ledger() -> MutexGuard<'static, Ledger> {
    match LEDGER.lock() {
        Ok(ledger) => ledger,
        Err(poisoned) => {
            let mut ledger = poisoned.into_inner();
            ledger.unknown_failure = true;
            ledger
        }
    }
}

fn arm_refusal(state: &mut Ledger) {
    let metric = &mcp_agent_mail_core::global_metrics()
        .storage
        .wbq_last_unrecoverable_error_us;
    if metric.load() != state.timestamp {
        // A legacy/untracked latch cannot be discharged by newer tracked work.
        state.unknown_failure = true;
    }
    let now = crate::now_micros_u64().max(1);
    state.timestamp = now.max(state.timestamp.saturating_add(1));
    metric.set(state.timestamp);
}

/// Pins automatic clearing while failure witnesses are being collected.
/// Canonicalization and bounded content hashing may be slow, so neither is
/// performed under the process-wide failure-ledger mutex. An interrupted
/// collector leaves an unknown obligation, not an apparently empty ledger.
struct Registration {
    counted: bool,
    finished: bool,
}

impl Registration {
    fn begin() -> Self {
        let mut state = ledger();
        arm_refusal(&mut state);
        let counted = if let Some(pending) = state.pending_registrations.checked_add(1) {
            state.pending_registrations = pending;
            true
        } else {
            state.unknown_failure = true;
            false
        };
        Self {
            counted,
            finished: false,
        }
    }

    fn record(&self, witness: Option<Witness>) {
        let mut state = ledger();
        // An explicit clear can race collection. Do not silently discard a
        // witness that was not yet available to that operator decision.
        if state.timestamp == 0 {
            arm_refusal(&mut state);
        }
        state.record(witness, Instant::now());
    }

    fn finish(mut self) {
        let mut state = ledger();
        if self.counted {
            state.pending_registrations -= 1;
        }
        self.finished = true;
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = ledger();
            arm_refusal(&mut state);
            state.unknown_failure = true;
            if self.counted {
                state.pending_registrations -= 1;
            }
        }
    }
}

/// Called at BOTH retry-exhaustion sites. Refusal is visible before any
/// operation is inspected; a pending registration prevents an earlier repair
/// from clearing the latch while these witnesses are still being collected.
pub(crate) fn record_failures<'a>(ops: impl IntoIterator<Item = &'a WriteOp>) {
    let registration = Registration::begin();
    let mut count = 0;
    for op in ops {
        count += 1;
        if count > MAX_FAILURES {
            registration.record(None);
            break;
        }
        let witness = Witness::from_op(op);
        registration.record(witness);
    }
    if count == 0 {
        registration.record(None);
    }
    registration.finish();
}

pub(crate) fn clear_by_operator() {
    let mut ledger = ledger();
    ledger.operator_clear();
    mcp_agent_mail_core::global_metrics()
        .storage
        .wbq_last_unrecoverable_error_us
        .set(0);
    // An explicit operator clear is the only authority to forgive a poisoned
    // failure ledger. Without this, every later lock would rearm unknown debt.
    LEDGER.clear_poison();
}

fn complete(ticket: &Ticket) {
    let metric = &mcp_agent_mail_core::global_metrics()
        .storage
        .wbq_last_unrecoverable_error_us;
    let mut ledger = ledger();
    if metric.load() != ledger.timestamp {
        ledger.unknown_failure = true;
        return;
    }
    if ledger.complete(ticket) {
        let failed_at = ledger.timestamp;
        ledger.timestamp = 0;
        metric.set(0);
        drop(ledger);
        tracing::info!(target: "maintenance", event = "wbq_durability_recovered",
            message_id = ticket.witness.id,
            failure_timestamp_us = failed_at,
            "all tracked WBQ failures verified in the archive; write refusal cleared");
    }
}

fn check_source(cx: &Cx, pool: &DbPool, config: &Config, ticket: &Ticket) -> Result<(), String> {
    validate_pool_binding(pool, config)?;
    if Scope::capture(config).as_ref() != Some(&ticket.witness.scope) {
        return Err("failed-message source identity changed; operator review required".into());
    }
    let conn = outcome(block_on(pool.acquire(cx)))?;
    let rows = conn
        .query_sync("PRAGMA query_only", &[])
        .map_err(source_error)?;
    if rows.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
        return Err("query-only snapshots cannot discharge a WBQ durability refusal".into());
    }
    Ok(())
}

fn disk_is_critical() -> bool {
    mcp_agent_mail_core::global_metrics()
        .system
        .disk_pressure_level
        .load()
        >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64()
}

fn ready_tickets(
    state: &Ledger,
    scope: &Scope,
    cursor: &ReconcileCursor,
    now: Instant,
) -> Vec<Ticket> {
    let limit = MAX_RETRY_IDS
        .saturating_sub(cursor.retries.len())
        .min(MAX_RETRIES_PER_PASS);
    let mut ready: Vec<Ticket> = Vec::with_capacity(limit);
    for entry in &state.entries {
        if ready.len() == limit {
            break;
        }
        let id = entry.witness.id;
        // Filter before applying the batch limit. A page of already-queued or
        // cooling IDs must not prevent later failed messages from being admitted.
        // Multiple failed versions of one ID retain distinct proof obligations,
        // but only need one source read on this pass.
        if &entry.witness.scope != scope
            || entry.ready_at > now
            || cursor.retries.iter().any(|retry| retry.id == id)
            || cursor
                .conflict_backoff
                .iter()
                .any(|(known, until)| *known == id && *until > now)
            || ready.iter().any(|ticket| ticket.witness.id == id)
        {
            continue;
        }
        ready.push(entry.clone());
    }
    ready
}

/// Add failed IDs to the EXISTING retry lane after its source has been bound.
/// No extra SQL page, publication budget or worker turn is introduced. Existing
/// retry/conflict cooldowns and the capacity bound remain authoritative.
pub(super) fn seed_retries(config: &Config, cursor: &mut ReconcileCursor) {
    if !crate::durability_degraded() || disk_is_critical() {
        return;
    }
    let Some(scope) = Scope::capture(config) else {
        return;
    };
    let now = Instant::now();
    let mut ledger = ledger();
    let ready = ready_tickets(&ledger, &scope, cursor, now);
    for ticket in ready {
        let id = ticket.witness.id;
        if cursor.retries.len() >= MAX_RETRY_IDS {
            break;
        }
        if cursor.retries.iter().any(|retry| retry.id == id)
            || cursor
                .conflict_backoff
                .iter()
                .any(|(known, until)| *known == id && *until > now)
        {
            continue;
        }
        cursor.defer_transient(id, now);
        cursor.next_lane_is_retry = true;
        // If an immutable-content mismatch prevents proof completion, do not
        // reinject the same ID on every fast catch-up turn. The normal retry
        // lane owns any failures during the intervening minute.
        ledger.postpone(&ticket, now);
    }
}

/// Snapshot the failure ticket BEFORE the existing archive publication runs.
/// A failure recorded during that publication gets a different sequence and
/// cannot be cleared by this older attempt, even when its content is identical.
pub(super) fn ticket_for(config: &Config, prepared: &PreparedMessage) -> Option<Ticket> {
    if !crate::durability_degraded() {
        return None;
    }
    let id = prepared.message.get("id")?.as_i64()?;
    if !ledger().entries.iter().any(|entry| entry.witness.id == id) {
        return None;
    }
    let scope = Scope::capture(config)?;
    let digest = fingerprint(
        &prepared.message,
        &prepared.body,
        &prepared.sender,
        &prepared.recipients,
    )?;
    ledger()
        .entries
        .iter()
        .find(|entry| {
            entry.witness.scope == scope
                && entry.witness.id == id
                && entry.witness.project == prepared.project_slug
                && entry.witness.digest == digest
        })
        .cloned()
}

/// Called only after reconcile_prepared_inner returns verified Git success,
/// while the caller still holds its original database-promotion exclusion.
/// Failed confirmation retains the independent latch obligation; it does not
/// misreport a real archive publication as a new send or change any DB row.
pub(super) fn confirm_repaired(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    source_identity: &str,
    stop: &AtomicBool,
    ticket: Option<&Ticket>,
) {
    let Some(ticket) = ticket else { return };
    let verified = (|| -> Result<(), String> {
        if stop.load(Ordering::Acquire)
            || cx.checkpoint().is_err()
            || disk_is_critical()
            || corruption_circuit_breaker().is_tripped()
        {
            return Err("durability proof completion interrupted".into());
        }
        check_source(cx, pool, config, ticket)?;
        let latest = prepare_message_classified(cx, pool, ticket.witness.id)
            .map_err(|error| error.detail)?;
        if pool.sqlite_identity_key() != source_identity || !ticket.witness.matches(&latest) {
            return Err("failed-message source changed during archive repair".into());
        }
        if stop.load(Ordering::Acquire)
            || cx.checkpoint().is_err()
            || disk_is_critical()
            || corruption_circuit_breaker().is_tripped()
        {
            return Err("durability proof completion interrupted".into());
        }
        Ok(())
    })();
    match verified {
        Ok(()) => complete(ticket),
        Err(reason) => {
            ledger().postpone(ticket, Instant::now());
            tracing::warn!(target: "maintenance", event = "wbq_durability_repair_deferred",
                message_id = ticket.witness.id, reason = %reason,
                "failed write remains unverified; preserving write refusal");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collection_releases_the_mutex_without_allowing_an_old_proof_to_clear() {
        super::super::tests::retention_fixture(1, |cx, pool, config| {
            clear_by_operator();
            let prepared = prepare_message_classified(cx, pool, 901).unwrap();
            let failed = op(config, &prepared);
            record_failures(std::iter::once(&failed));
            let old = ledger().entries[0].clone();

            record_failures(std::iter::once_with(|| {
                // This callback runs after failure admission but before its
                // witness is collected. It must not inherit the ledger mutex.
                let state = match LEDGER.try_lock() {
                    Ok(state) => state,
                    Err(_) => panic!("collection retained the ledger mutex"),
                };
                assert_eq!(state.pending_registrations, 1);
                drop(state);
                complete(&old);
                assert!(
                    crate::durability_degraded(),
                    "a pending failure has no proof yet"
                );
                assert!(ledger().entries.is_empty());
                &failed
            }));

            assert_eq!(ledger().pending_registrations, 0);
            assert_eq!(ledger().entries.len(), 1);
            assert_ne!(ledger().entries[0].sequence, old.sequence);
            drive(cx, pool, config, &AtomicBool::new(false)).unwrap();
            assert!(!crate::durability_degraded());
            assert_committed(config, &prepared);
        });
    }

    #[test]
    fn interrupted_collection_retains_unknown_failure_without_poisoning_the_mutex() {
        super::super::tests::retention_fixture(1, |_, _, _| {
            clear_by_operator();
            let result = std::panic::catch_unwind(|| {
                record_failures(std::iter::from_fn(|| -> Option<&WriteOp> {
                    panic!("failure while collecting a write witness");
                }));
            });
            assert!(result.is_err());
            assert!(crate::durability_degraded());
            assert!(!LEDGER.is_poisoned());
            let state = ledger();
            assert_eq!(state.pending_registrations, 0);
            assert!(state.unknown_failure);
            drop(state);
            clear_by_operator();
        });
    }

    #[test]
    fn operator_clear_during_collection_does_not_discard_the_unseen_witness() {
        super::super::tests::retention_fixture(1, |cx, pool, config| {
            clear_by_operator();
            let prepared = prepare_message_classified(cx, pool, 901).unwrap();
            let failed = op(config, &prepared);
            record_failures(std::iter::once_with(|| {
                clear_by_operator();
                assert_eq!(ledger().pending_registrations, 1);
                &failed
            }));
            assert!(crate::durability_degraded());
            assert_eq!(ledger().pending_registrations, 0);
            assert_eq!(ledger().entries.len(), 1);
            drive(cx, pool, config, &AtomicBool::new(false)).unwrap();
            assert!(!crate::durability_degraded());
            assert_committed(config, &prepared);
        });
    }

    /// Execute the real WBQ receiver, retry loop and failure sites. A private
    /// channel makes the ordering deterministic without substituting storage,
    /// failure accounting, or the registration/clear callbacks.
    fn drain_ops(ops: Vec<WriteOp>, shutdown_first: bool, batch_cap: usize) {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        let capacity = ops.len() + 1;
        let (sender, receiver) = std::sync::mpsc::sync_channel(capacity);
        let depth = Arc::new(AtomicU64::new(0));
        if shutdown_first {
            assert!(sender.send(crate::WbqMsg::Shutdown).is_ok());
        }
        for op in ops {
            let message = crate::WbqMsg::Op(crate::WbqOpEnvelope {
                enqueued_at: Instant::now(),
                op: Box::new(op),
            });
            assert!(crate::wbq_try_send_counted(&sender, &depth, message, None).is_ok());
        }
        if !shutdown_first {
            assert!(sender.send(crate::WbqMsg::Shutdown).is_ok());
        }
        drop(sender);
        crate::wbq_drain_loop(
            Arc::new(Mutex::new(Some(receiver))),
            depth.clone(),
            capacity,
            batch_cap,
        );
        assert_eq!(depth.load(Ordering::Relaxed), 0);
        crate::flush_async_commits();
    }

    fn exercise_real_drain_failure(count: i64, shutdown_first: bool) {
        super::super::tests::retention_fixture(count, |cx, pool, config| {
            clear_by_operator();
            let prepared = (901..901 + count)
                .map(|id| prepare_message_classified(cx, pool, id).unwrap())
                .collect::<Vec<_>>();
            let archive = crate::ensure_archive(config, "project").unwrap();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &prepared[0].message,
                &prepared[0].sender,
                &prepared[0].recipients,
            )
            .unwrap()
            .0;
            // A directory where the canonical regular file must go forces
            // the production archive writer to fail; nothing fakes its Result.
            std::fs::create_dir_all(&paths.canonical).unwrap();
            let before = crate::wbq_stats();
            let mut ops = prepared
                .iter()
                .map(|message| op(config, message))
                .collect::<Vec<_>>();
            let expected_failures = if shutdown_first {
                // Shutdown is received first; a healthy 902 consumes the
                // one-op main batch. Failed 901 is then executed by the actual
                // post-shutdown drain, exercising its distinct failure hook.
                assert_eq!(count, 2);
                ops.reverse();
                1
            } else {
                u64::try_from(count).unwrap()
            };
            drain_ops(
                ops,
                shutdown_first,
                if shutdown_first {
                    1
                } else {
                    usize::try_from(count).unwrap()
                },
            );
            let failed = crate::wbq_stats();
            assert_eq!(
                failed.unrecoverable_errors - before.unrecoverable_errors,
                expected_failures
            );
            assert_eq!(failed.errors - before.errors, expected_failures);
            assert_eq!(
                failed.drained - before.drained,
                u64::try_from(count).unwrap()
            );
            assert!(
                crate::durability_degraded(),
                "the drain must register its failure itself"
            );
            assert_eq!(
                ledger().entries.len(),
                usize::try_from(expected_failures).unwrap()
            );
            assert_eq!(ledger().pending_registrations, 0);
            assert!(!ledger().unknown_failure);

            std::fs::rename(
                &paths.canonical,
                config.storage_root.join("retained-wbq-obstruction"),
            )
            .unwrap();
            drive(cx, pool, config, &AtomicBool::new(false)).unwrap();
            assert!(
                !crate::durability_degraded(),
                "verified repair must clear the real drain failure"
            );
            assert_eq!(
                crate::wbq_stats().unrecoverable_errors,
                failed.unrecoverable_errors
            );
            for message in &prepared {
                assert_committed(config, message);
            }
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn
                .query_sync("SELECT COUNT(*) AS n FROM messages", &[])
                .unwrap();
            assert_eq!(rows[0].get_named::<i64>("n").unwrap(), count);
            let rows = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients ORDER BY message_id, agent_id",
                    &[],
                )
                .unwrap();
            assert_eq!(rows.len(), usize::try_from(count * 2).unwrap());
            for row in rows {
                assert_eq!(row.get_named::<i64>("read_ts").unwrap(), 2_000_000);
                assert_eq!(row.get_named::<i64>("ack_ts").unwrap(), 2_500_000);
            }
        });
    }

    #[test]
    fn real_wbq_single_failure_recovers_without_operator_clear() {
        exercise_real_drain_failure(1, false);
    }

    #[test]
    fn real_wbq_batched_failure_tracks_and_verifies_every_failed_operation() {
        exercise_real_drain_failure(2, false);
    }

    #[test]
    fn real_wbq_post_shutdown_failure_is_not_lost_by_the_second_drain_path() {
        exercise_real_drain_failure(2, true);
    }

    fn witness(id: i64) -> Witness {
        Witness {
            scope: Scope {
                database: PathBuf::from("/fixture/mail.sqlite3"),
                archive: PathBuf::from("/fixture/archive"),
                file_identity: "fixture-inode".into(),
            },
            project: "project".into(),
            id,
            digest: [17; 32],
        }
    }

    #[test]
    fn repeated_failure_invalidates_an_in_flight_success_ticket() {
        let mut state = Ledger::default();
        let now = Instant::now();
        state.record(Some(witness(1)), now);
        let stale = state.entries[0].clone();
        state.record(Some(witness(1)), now);
        assert_eq!(state.entries.len(), 1);
        assert!(!state.complete(&stale));
        assert_eq!(state.entries.len(), 1);
        let fresh = state.entries[0].clone();
        assert!(state.complete(&fresh));
    }

    #[test]
    fn every_scope_and_every_failed_content_version_must_be_resolved() {
        let mut state = Ledger::default();
        let now = Instant::now();
        state.record(Some(witness(1)), now);
        let mut other = witness(1);
        other.scope.archive = PathBuf::from("/fixture/other");
        state.record(Some(other), now);
        let mut changed = witness(1);
        changed.digest[0] ^= 1;
        state.record(Some(changed), now);
        let tickets = state.entries.iter().cloned().collect::<Vec<_>>();
        assert!(!state.complete(&tickets[0]));
        assert!(!state.complete(&tickets[1]));
        assert!(state.complete(&tickets[2]));
    }

    #[test]
    fn unknown_failure_keeps_the_refusal_after_all_known_repairs() {
        let mut state = Ledger::default();
        state.record(Some(witness(1)), Instant::now());
        state.record(None, Instant::now());
        let ticket = state.entries[0].clone();
        assert!(!state.complete(&ticket));
        assert!(state.unknown_failure);
        assert!(state.entries.is_empty());
    }

    #[test]
    fn capacity_exhaustion_does_not_evict_unresolved_failures_or_allow_a_clear() {
        let mut state = Ledger::default();
        let now = Instant::now();
        for id in 1..=i64::try_from(MAX_FAILURES + 1).unwrap() {
            state.record(Some(witness(id)), now);
        }
        assert_eq!(state.entries.len(), MAX_FAILURES);
        assert!(state.unknown_failure);
        assert_eq!(state.entries.front().unwrap().witness.id, 1);
        let tickets = state.entries.iter().cloned().collect::<Vec<_>>();
        for ticket in tickets {
            assert!(!state.complete(&ticket));
        }
    }

    #[test]
    fn operator_clear_cannot_make_an_old_ticket_valid_for_new_failures() {
        let mut state = Ledger::default();
        let now = Instant::now();
        state.record(Some(witness(1)), now);
        let old = state.entries[0].clone();
        state.operator_clear();
        state.record(Some(witness(1)), now);
        assert!(!state.complete(&old));
        assert_eq!(state.entries.len(), 1);
        let current = state.entries[0].clone();
        assert!(state.complete(&current));
    }

    #[test]
    fn retry_backoff_rotates_work_without_consuming_the_obligation() {
        let mut state = Ledger::default();
        let now = Instant::now();
        state.record(Some(witness(1)), now);
        state.record(Some(witness(2)), now);
        let ticket = state.entries[0].clone();
        state.postpone(&ticket, now);
        assert_eq!(state.entries.front().unwrap().witness.id, 2);
        assert_eq!(state.entries.back().unwrap().ready_at, now + RETRY_DELAY);
        assert_eq!(state.entries.len(), 2);
        let mut stale = ticket.clone();
        stale.sequence = u64::MAX;
        state.postpone(&stale, now + RETRY_DELAY);
        assert_eq!(state.entries.back().unwrap().ready_at, now + RETRY_DELAY);
    }

    #[test]
    fn ticket_sequence_exhaustion_pins_manual_recovery() {
        let mut state = Ledger {
            next_sequence: u64::MAX,
            ..Default::default()
        };
        state.record(Some(witness(1)), Instant::now());
        assert!(state.unknown_failure);
        assert!(state.entries.is_empty());
    }

    #[test]
    fn already_queued_and_cooling_ids_do_not_starve_later_failures() {
        let now = Instant::now();
        let mut state = Ledger::default();
        let mut cursor = ReconcileCursor::default();
        for id in 1..=40 {
            state.record(Some(witness(id)), now);
            if id <= 16 {
                cursor.defer_transient(id, now);
            } else if id <= 32 {
                cursor.defer_conflict(id, now);
            }
        }
        let ready = ready_tickets(&state, &witness(1).scope, &cursor, now);
        assert_eq!(
            ready
                .iter()
                .map(|ticket| ticket.witness.id)
                .collect::<Vec<_>>(),
            (33..=40).collect::<Vec<_>>(),
        );
        assert_eq!(state.entries.len(), 40, "selection never discharges debt");
    }

    #[test]
    fn admission_deduplicates_ids_and_respects_remaining_retry_capacity() {
        let now = Instant::now();
        let mut state = Ledger::default();
        state.record(Some(witness(1)), now);
        let mut changed = witness(1);
        changed.digest[0] ^= 1;
        state.record(Some(changed), now);
        state.record(Some(witness(2)), now);
        let mut cursor = ReconcileCursor::default();
        let scope = witness(1).scope;
        assert_eq!(ready_tickets(&state, &scope, &cursor, now).len(), 2);
        for id in 10_000..10_000 + i64::try_from(MAX_RETRY_IDS - 1).unwrap() {
            cursor.defer_transient(id, now);
        }
        assert_eq!(ready_tickets(&state, &scope, &cursor, now).len(), 1);
        cursor.defer_transient(99_999, now);
        assert!(ready_tickets(&state, &scope, &cursor, now).is_empty());
        assert_eq!(state.entries.len(), 3);
    }

    #[test]
    fn digest_is_key_order_independent_but_binds_body_identity_and_private_routing() {
        let left: Value =
            serde_json::from_str(r#"{"id":1,"nested":{"a":1,"b":2},"bcc":["RedFox"]}"#).unwrap();
        let right: Value =
            serde_json::from_str(r#"{"bcc":["RedFox"],"nested":{"b":2,"a":1},"id":1}"#).unwrap();
        let recipients = vec!["BlueLake".into(), "RedFox".into()];
        let original = fingerprint(&left, "body", "GoldFox", &recipients).unwrap();
        assert_eq!(
            Some(original),
            fingerprint(&right, "body", "GoldFox", &recipients)
        );
        assert_ne!(
            Some(original),
            fingerprint(&left, "changed", "GoldFox", &recipients)
        );
        assert_ne!(
            Some(original),
            fingerprint(&left, "body", "SilverCloud", &recipients)
        );
        let mut changed = left.clone();
        changed["bcc"] = json!([]);
        assert_ne!(
            Some(original),
            fingerprint(&changed, "body", "GoldFox", &recipients)
        );
        assert_ne!(
            Some(original),
            fingerprint(&left, "body", "GoldFox", &recipients[..1])
        );
        let mut reversed = recipients.clone();
        reversed.reverse();
        reversed.push(recipients[0].clone());
        assert_eq!(
            Some(original),
            fingerprint(&left, "body", "GoldFox", &reversed)
        );
        let oversized = "x".repeat(usize::try_from(MAX_DB_PAYLOAD_BYTES).unwrap() + 1);
        assert!(fingerprint(&left, &oversized, "GoldFox", &recipients).is_none());
    }

    // retention_fixture already re-executes each real DB/Git test in its own
    // process with a watchdog, isolating this process-global latch and ledger.

    fn op(config: &Config, prepared: &PreparedMessage) -> WriteOp {
        WriteOp::MessageBundle {
            config: config.clone(),
            project_slug: prepared.project_slug.clone(),
            message_json: prepared.message.clone(),
            body_md: prepared.body.clone(),
            sender: prepared.sender.clone(),
            recipients: prepared.recipients.clone(),
            extra_paths: Vec::new(),
        }
    }

    fn drive(
        cx: &Cx,
        pool: &DbPool,
        config: &Config,
        stop: &AtomicBool,
    ) -> Result<super::super::ReconcileReport, String> {
        // Exercise the same bounded selection, promotion exclusion, publication,
        // and confirmation path as maintenance. Never clear the latch directly.
        super::super::reconcile_message_batch(
            cx,
            pool,
            config,
            &mut ReconcileCursor::default(),
            stop,
        )
    }

    fn assert_committed(config: &Config, prepared: &PreparedMessage) {
        let archive = crate::ensure_archive(config, &prepared.project_slug).unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &prepared.message,
            &prepared.sender,
            &prepared.recipients,
        )
        .unwrap()
        .0;
        let committed = super::super::CommittedMessages::open(&archive).unwrap();
        for path in std::iter::once(&paths.canonical)
            .chain(std::iter::once(&paths.outbox))
            .chain(paths.inbox.iter())
        {
            let (message, body) = committed.read(&archive, path).unwrap().unwrap();
            let on_disk = super::super::read_surviving_message(path).unwrap().unwrap();
            assert_eq!(on_disk, (message.clone(), body.clone()));
            assert_eq!(body, prepared.body);
            let mut expected = prepared.message.clone();
            if paths.inbox.contains(path) {
                expected["bcc"] = json!([]);
            }
            assert_eq!(message, expected, "{}", path.display());
        }
    }

    #[test]
    fn missing_source_row_cannot_clear_a_tracked_write_failure() {
        super::super::tests::retention_fixture(1, |cx, pool, config| {
            clear_by_operator();
            let prepared = prepare_message_classified(cx, pool, 901).unwrap();
            record_failures(std::iter::once(&op(config, &prepared)));
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("DELETE FROM message_recipients WHERE message_id = 901")
                .unwrap();
            conn.execute_raw("DELETE FROM messages WHERE id = 901")
                .unwrap();
            drop(conn);
            let report = drive(cx, pool, config, &AtomicBool::new(false)).unwrap();
            assert_eq!(report.repaired, 0);
            assert!(crate::durability_degraded());
            assert_eq!(ledger().entries.len(), 1);
            clear_by_operator();
        });
    }

    #[test]
    fn changed_source_content_cannot_discharge_the_original_write_failure() {
        super::super::tests::retention_fixture(1, |cx, pool, config| {
            clear_by_operator();
            let prepared = prepare_message_classified(cx, pool, 901).unwrap();
            record_failures(std::iter::once(&op(config, &prepared)));
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE messages SET body_md = 'Different content' WHERE id = 901")
                .unwrap();
            drop(conn);
            drive(cx, pool, config, &AtomicBool::new(false)).unwrap();
            assert!(
                crate::durability_degraded(),
                "repairing different content is not proof of the failed write"
            );
            assert_eq!(ledger().entries.len(), 1);
            clear_by_operator();
        });
    }

    #[test]
    fn cancelled_pass_preserves_failure_until_a_later_verified_repair() {
        super::super::tests::retention_fixture(1, |cx, pool, config| {
            clear_by_operator();
            let prepared = prepare_message_classified(cx, pool, 901).unwrap();
            record_failures(std::iter::once(&op(config, &prepared)));
            let stopped = drive(cx, pool, config, &AtomicBool::new(true)).unwrap();
            assert!(stopped.interrupted);
            assert!(crate::durability_degraded());
            assert_eq!(ledger().entries.len(), 1);
            let repaired = drive(cx, pool, config, &AtomicBool::new(false)).unwrap();
            assert_eq!(repaired.repaired, 1);
            assert!(!crate::durability_degraded());
            assert_committed(config, &prepared);
        });
    }
}
