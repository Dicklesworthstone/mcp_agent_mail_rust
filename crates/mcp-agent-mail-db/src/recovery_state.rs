//! Durable read/ack and contact state for archive-only reconstruction (GH#404).
//!
//! A bounded, streamed baseline is published once per database generation.
//! Subsequent mutations replace only their affected records. Publication takes
//! the same inter-process lock before rereading committed database state, so a
//! delayed contact approval cannot overwrite a newer block. No database or
//! readable salvage source is needed to consume the published state.

use crate::error::{DbError, DbResult};
use crate::pool::SyncQuery;
use mcp_agent_mail_core::journal_io::{JournalDirectory, JournalFileMode};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sqlmodel_core::{Row, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufRead as _, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

pub const DIRECTORY: &str = "recovery-state";
const MANIFEST_FILE: &str = "active.json";
const BASELINE_FILE: &str = "baseline.jsonl";
const SUPERSEDED_FILE: &str = "superseded.json";
const LOCK_FILE: &str = ".state.lock";
const READ_REVISION_FILE: &str = "read-revision.json";
// Runtime-only, write-ahead debt shared by all receipt/contact publishers.
// In-memory hints wake the coalescer; they are never completeness authority.
const GIT_PENDING_FILE: &str = ".git-pending.jsonl";
const MAX_GIT_PENDING_RECORD_BYTES: u64 = 1024;
const MAX_GIT_PENDING_BYTES: u64 = 64 * 1024 * 1024;
const SCHEMA_VERSION: u32 = 1;
const READ_REVISION_VERSION: u32 = 2;
const BATCH_ROWS: usize = 256;
const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;
const MAX_READ_REVISION_BYTES: u64 = 4096;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const ERROR_PREFIX: &str = "archive recovery state ";

static PENDING_ARCHIVE_PATHS: LazyLock<Mutex<BTreeMap<PathBuf, BTreeSet<String>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[cfg(test)]
thread_local! {
    static AFTER_RECORD_INTENT_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static AFTER_RECORD_REPLACE_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static AFTER_CHECKPOINT_ACTIVATION_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Hand completed state artifacts to the normal Git commit coalescer.
///
/// Durable files remain recoverable even if the process exits before this hint drains;
/// startup requeues the active generation without trusting an in-memory queue.
pub fn note_pending_archive_paths(root: &Path, paths: impl IntoIterator<Item = String>) {
    let mut pending = PENDING_ARCHIVE_PATHS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pending.entry(root.to_path_buf()).or_default().extend(paths);
}

/// Take pending root-relative paths. A publisher must requeue failed groups.
pub fn take_pending_archive_paths() -> Vec<(PathBuf, Vec<String>)> {
    let mut pending = PENDING_ARCHIVE_PATHS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut *pending)
        .into_iter()
        .map(|(root, paths)| (root, paths.into_iter().collect()))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectIdentity {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub id: i64,
    pub name: String,
    pub project: ProjectIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptRecord {
    pub message_id: i64,
    pub message_project: ProjectIdentity,
    pub message_created_ts: i64,
    pub message_subject: String,
    pub agent: AgentIdentity,
    pub shared: bool,
    pub read_ts: Option<i64>,
    pub ack_ts: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactRecord {
    pub id: i64,
    pub from: AgentIdentity,
    pub to: AgentIdentity,
    #[serde(deserialize_with = "deserialize_contact_status")]
    pub status: String,
    pub reason: String,
    pub created_ts: i64,
    pub updated_ts: i64,
    pub expires_ts: Option<i64>,
}

fn deserialize_contact_status<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    String::deserialize(deserializer).map(|status| status.trim().to_ascii_lowercase())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "state", rename_all = "snake_case")]
pub enum RecoveryRecord {
    Project(ProjectIdentity),
    Agent(AgentIdentity),
    Receipt(ReceiptRecord),
    Contact(ContactRecord),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub generation: String,
    pub baseline_sha256: String,
    pub projects: u64,
    pub agents: u64,
    pub receipts: u64,
    pub contacts: u64,
}

/// Opaque, bounded witness for reusing an archive read snapshot. Equality
/// binds the completed publication token to its exact active checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReadWitness {
    manifest: Manifest,
    revision: String,
    inventory: IncrementalInventory,
}

impl RecoveryReadWitness {
    /// Logical database generation whose completed recovery state this witness covers.
    #[must_use]
    pub fn generation(&self) -> &str {
        &self.manifest.generation
    }

    pub(crate) fn matches_manifest(&self, manifest: &Manifest) -> bool {
        &self.manifest == manifest
    }

    pub(crate) fn matches_inventory(&self, inventory: &IncrementalInventory) -> bool {
        &self.inventory == inventory
    }
}

/// Order-independent omission/corruption checksum over the current incremental
/// files. Each contribution is SHA-256 of its filename and exact bytes; count
/// distinguishes an omitted record even if its contribution were all zero.
/// This XOR accumulator is not an authenticated or collision-resistant
/// commitment against adversarially chosen sets. It protects partial/damaged
/// archives while keeping normal publication proportional to the changed keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IncrementalInventory {
    generation: String,
    count: u64,
    xor_sha256: [u8; 32],
}

impl IncrementalInventory {
    fn empty(generation: &str) -> Self {
        Self {
            generation: generation.to_string(),
            count: 0,
            xor_sha256: [0; 32],
        }
    }

    fn replace(&mut self, before: Option<&[u8; 32]>, after: &[u8; 32]) -> DbResult<()> {
        if let Some(before) = before {
            if self.count == 0 {
                return Err(state_error(
                    "incremental inventory",
                    "replacement has no recorded key",
                ));
            }
            for (sum, byte) in self.xor_sha256.iter_mut().zip(before) {
                *sum ^= *byte;
            }
        } else {
            self.count = self
                .count
                .checked_add(1)
                .ok_or_else(|| state_error("incremental inventory", "record count overflow"))?;
        }
        for (sum, byte) in self.xor_sha256.iter_mut().zip(after) {
            *sum ^= *byte;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingRecord {
    name: String,
    before: Option<[u8; 32]>,
    after: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRevision {
    schema_version: u32,
    generation: String,
    revision: String,
    /// None invalidates old snapshots before any journal record can change.
    /// An interrupted publication stays uncacheable until a successful retry.
    manifest: Option<Manifest>,
    /// The previous complete set, advanced only after a changed file is durable.
    /// During a generation switch this may still belong to the prior manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inventory: Option<IncrementalInventory>,
    /// A single write-ahead transition: on interruption the file must match its
    /// exact before or after checksum before any later writer may complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_record: Option<PendingRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecordDocument {
    schema_version: u32,
    generation: String,
    record: RecoveryRecord,
}

fn state_error(context: &str, error: impl std::fmt::Display) -> DbError {
    DbError::Internal(format!(
        "{ERROR_PREFIX}{context}: {error}; recovery completeness is not confirmed"
    ))
}

/// Distinguish failed durable recovery publication from an ordinary best-effort
/// database operation.
///
/// The database receipt may already have committed: after
/// archive access is repaired, explicitly retry its read or ACK to finish
/// publication without replacing the original receipt timestamps.
#[must_use]
pub fn is_publication_error(error: &DbError) -> bool {
    matches!(error, DbError::Internal(message) if message.starts_with(ERROR_PREFIX))
}

fn query(conn: &impl SyncQuery, sql: &str, params: &[Value]) -> DbResult<Vec<Row>> {
    conn.query_sync(sql, params)
        .map_err(|error| state_error("database query", error))
}

fn generation_directory(generation: &str) -> DbResult<String> {
    if generation.is_empty()
        || generation.len() > 256
        || !generation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(state_error(
            "generation",
            "invalid database generation token",
        ));
    }
    Ok(format!("g{generation}"))
}

/// Read the persisted generation, seeding a fresh migrated database once.
/// Existing malformed identities are never replaced with a fabricated token.
pub fn ensure_generation(conn: &impl SyncQuery) -> DbResult<String> {
    let mut rows = query(conn, crate::queries::SELECT_DB_GENERATION_SQL, &[])?;
    if rows.is_empty() {
        let token = mcp_agent_mail_core::setup::generate_token()
            .map_err(|error| state_error("generation token", error))?;
        generation_directory(&token)?;
        conn.execute_raw(&format!(
            "INSERT OR IGNORE INTO db_identity (singleton, generation_id) VALUES (0, '{token}')"
        ))
        .map_err(|error| state_error("generation seed", error))?;
        rows = query(conn, crate::queries::SELECT_DB_GENERATION_SQL, &[])?;
    }
    let generation: String = rows
        .first()
        .ok_or_else(|| state_error("generation", "database identity is absent"))?
        .get_named("generation_id")
        .map_err(|error| state_error("generation", error))?;
    generation_directory(&generation)?;
    Ok(generation)
}

struct LockedState {
    directory: JournalDirectory,
    lock: File,
}

impl LockedState {
    fn open(root: &Path, create: bool) -> DbResult<Self> {
        let directory = JournalDirectory::open(root, DIRECTORY, create)
            .map_err(|error| state_error("directory", error))?;
        let lock = directory
            .open_file(LOCK_FILE, JournalFileMode::Lock)
            .map_err(|error| state_error("lock file", error))?;
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            match fs2::FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(state_error("lock acquisition", error)),
            }
        }
        directory
            .validate_file(LOCK_FILE, &lock)
            .map_err(|error| state_error("lock binding", error))?;
        Ok(Self { directory, lock })
    }

    fn validate(&self) -> DbResult<()> {
        self.directory
            .validate_file(LOCK_FILE, &self.lock)
            .map_err(|error| state_error("lock binding", error))
    }

    fn generation(&self, generation: &str, create: bool) -> DbResult<JournalDirectory> {
        self.validate()?;
        JournalDirectory::open(
            &self.directory.directory_path(),
            &generation_directory(generation)?,
            create,
        )
        .map_err(|error| state_error("generation directory", error))
    }
}

fn read_json<T: serde::de::DeserializeOwned>(
    directory: &JournalDirectory,
    name: &str,
) -> DbResult<Option<T>> {
    read_json_bounded(directory, name, MAX_RECORD_BYTES)
}

fn read_json_bounded<T: serde::de::DeserializeOwned>(
    directory: &JournalDirectory,
    name: &str,
    limit: u64,
) -> DbResult<Option<T>> {
    read_bytes_bounded(directory, name, limit)?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|error| state_error("decode", error)))
        .transpose()
}

fn read_bytes_bounded(
    directory: &JournalDirectory,
    name: &str,
    limit: u64,
) -> DbResult<Option<Vec<u8>>> {
    let mut file = match directory.open_file(name, JournalFileMode::Read) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(state_error("read", error)),
    };
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| state_error("read", error))?;
    if bytes.len() as u64 > limit {
        return Err(state_error("read", "record exceeds bounded size limit"));
    }
    directory
        .validate_file(name, &file)
        .map_err(|error| state_error("read binding", error))?;
    Ok(Some(bytes))
}

fn validate_manifest(manifest: &Manifest) -> DbResult<()> {
    generation_directory(&manifest.generation)?;
    if manifest.schema_version != SCHEMA_VERSION
        || manifest.baseline_sha256.len() != 64
        || !manifest
            .baseline_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(state_error(
            "manifest",
            "invalid version or baseline checksum",
        ));
    }
    Ok(())
}

fn read_manifest(directory: &JournalDirectory) -> DbResult<Option<Manifest>> {
    let manifest = read_json::<Manifest>(directory, MANIFEST_FILE)?;
    if let Some(manifest) = &manifest {
        validate_manifest(manifest)?;
    }
    Ok(manifest)
}

fn read_revision(directory: &JournalDirectory) -> DbResult<Option<ReadRevision>> {
    let revision =
        read_json_bounded::<ReadRevision>(directory, READ_REVISION_FILE, MAX_READ_REVISION_BYTES)?;
    if let Some(revision) = &revision {
        generation_directory(&revision.generation)?;
        generation_directory(&revision.revision)?;
        match revision.schema_version {
            1 if revision.inventory.is_none() && revision.pending_record.is_none() => {}
            READ_REVISION_VERSION => {
                let inventory = revision.inventory.as_ref().ok_or_else(|| {
                    state_error("read revision", "incremental inventory is missing")
                })?;
                generation_directory(&inventory.generation)?;
                if inventory.count == 0 && inventory.xor_sha256 != [0; 32] {
                    return Err(state_error(
                        "read revision",
                        "empty incremental inventory has a nonempty checksum",
                    ));
                }
                if let Some(pending) = &revision.pending_record {
                    validate_incremental_name(&pending.name)?;
                    if pending.before.as_ref() == Some(&pending.after)
                        || (pending.before.is_some() && inventory.count == 0)
                    {
                        return Err(state_error(
                            "read revision",
                            "invalid pending record transition",
                        ));
                    }
                }
                if revision.manifest.is_some()
                    && (inventory.generation != revision.generation
                        || revision.pending_record.is_some())
                {
                    return Err(state_error(
                        "read revision",
                        "completed inventory differs from its generation or has pending writes",
                    ));
                }
            }
            _ => return Err(state_error("read revision", "unsupported schema version")),
        }
        if let Some(manifest) = &revision.manifest {
            validate_manifest(manifest)?;
            if manifest.generation != revision.generation {
                return Err(state_error(
                    "read revision",
                    "checkpoint generation mismatch",
                ));
            }
        }
    }
    Ok(revision)
}

/// Read a constant-size cache witness without changing files or walking
/// journal history.
///
/// A missing or unsealed legacy revision, or pending publication, returns
/// `None`: callers may rebuild for inspection but must not reuse a cached
/// receipt snapshot. Malformed, unreadable or mismatched metadata is an
/// error, never evidence that a previously cached snapshot remains current.
pub fn read_revision_witness(root: &Path) -> DbResult<Option<RecoveryReadWitness>> {
    let directory = match JournalDirectory::open(root, DIRECTORY, false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(state_error("read revision directory", error)),
    };
    let Some(before) = read_revision(&directory)? else {
        return Ok(None);
    };
    let (Some(expected_manifest), Some(inventory)) = (&before.manifest, &before.inventory) else {
        return Ok(None);
    };
    let manifest =
        read_json_bounded::<Manifest>(&directory, MANIFEST_FILE, MAX_READ_REVISION_BYTES)?
            .ok_or_else(|| state_error("read revision", "active checkpoint is missing"))?;
    validate_manifest(&manifest)?;
    if read_revision(&directory)?.as_ref() != Some(&before) {
        return Ok(None);
    }
    if &manifest != expected_manifest {
        return Err(state_error(
            "read revision",
            "active checkpoint differs from the completed publication",
        ));
    }
    Ok(Some(RecoveryReadWitness {
        manifest,
        revision: before.revision.clone(),
        inventory: inventory.clone(),
    }))
}

fn write_revision(state: &LockedState, revision: &ReadRevision) -> DbResult<()> {
    state.validate()?;
    let bytes =
        serde_json::to_vec(revision).map_err(|error| state_error("read revision encode", error))?;
    state
        .directory
        .atomic_write(READ_REVISION_FILE, |file| file.write_all(&bytes))
        .map_err(|error| state_error("read revision publication", error))?;
    state.validate()
}

fn with_read_revision<T>(
    state: &LockedState,
    conn: &impl SyncQuery,
    root: &Path,
    generation: &str,
    publish: impl FnOnce(&mut ReadRevision) -> DbResult<T>,
) -> DbResult<T> {
    // Validate existing authority before writing even an invalidation marker.
    // In particular, a malformed manifest must retain its original evidence.
    let active = read_manifest(&state.directory)?;
    validate_checkpoint_generation(state, generation, active.as_ref())?;
    let previous_revision = read_revision(&state.directory)?;
    if let Some(expected) = previous_revision
        .as_ref()
        .and_then(|revision| revision.manifest.as_ref())
        && active.as_ref() != Some(expected)
    {
        return Err(state_error(
            "read revision",
            "active checkpoint differs from the completed publication",
        ));
    }
    let reconcile_pending = active.is_some()
        && previous_revision
            .as_ref()
            .is_none_or(|revision| revision.manifest.is_none() || revision.inventory.is_none());
    let inventory = publication_inventory(
        state,
        active.as_ref(),
        generation,
        previous_revision.as_ref(),
    )?;
    let mut revision = ReadRevision {
        schema_version: READ_REVISION_VERSION,
        generation: generation.to_string(),
        revision: mcp_agent_mail_core::setup::generate_token()
            .map_err(|error| state_error("read revision token", error))?,
        manifest: None,
        inventory: Some(inventory),
        pending_record: None,
    };
    ensure_git_pending_journal(state)?;
    write_revision(state, &revision)?;
    let manifest = checkpoint_locked(state, conn, root, generation)?;
    #[cfg(test)]
    if active
        .as_ref()
        .is_none_or(|active| active.generation != manifest.generation)
    {
        let hook = AFTER_CHECKPOINT_ACTIVATION_TEST_HOOK.with(|hook| hook.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }
    if revision
        .inventory
        .as_ref()
        .is_none_or(|inventory| inventory.generation != manifest.generation)
    {
        // A newly activated baseline contains all initial state. Generation
        // validation already refused any preexisting incremental records.
        revision.inventory = Some(IncrementalInventory::empty(&manifest.generation));
        write_revision(state, &revision)?;
    }
    if reconcile_pending {
        // The expected inventory was verified before reconciliation. A later
        // successful write must neither hide missing old records nor forget an
        // earlier receipt/contact publication that stopped partway through.
        reconcile_incomplete_publication(state, conn, root, generation, &mut revision)?;
    }
    let result = publish(&mut revision)?;
    let manifest = read_manifest(&state.directory)?
        .ok_or_else(|| state_error("read revision", "publication has no active checkpoint"))?;
    if manifest.generation != generation
        || revision
            .inventory
            .as_ref()
            .is_none_or(|inventory| inventory.generation != generation)
        || revision.pending_record.is_some()
    {
        return Err(state_error(
            "read revision",
            "publication changed the active database generation or left a pending record",
        ));
    }
    revision.manifest = Some(manifest);
    write_revision(state, &revision)?;
    note_pending_archive_paths(root, [format!("{DIRECTORY}/{READ_REVISION_FILE}")]);
    Ok(result)
}

fn project(row: &Row, prefix: &str) -> DbResult<ProjectIdentity> {
    Ok(ProjectIdentity {
        id: row
            .get_named(&format!("{prefix}project_id"))
            .map_err(|error| state_error("project identity", error))?,
        slug: row
            .get_named(&format!("{prefix}project_slug"))
            .map_err(|error| state_error("project identity", error))?,
        human_key: row
            .get_named(&format!("{prefix}project_human_key"))
            .map_err(|error| state_error("project identity", error))?,
    })
}

fn agent(row: &Row, prefix: &str) -> DbResult<AgentIdentity> {
    Ok(AgentIdentity {
        id: row
            .get_named(&format!("{prefix}agent_id"))
            .map_err(|error| state_error("agent identity", error))?,
        name: row
            .get_named(&format!("{prefix}agent_name"))
            .map_err(|error| state_error("agent identity", error))?,
        project: project(row, prefix)?,
    })
}

fn receipt_select(shared: bool) -> String {
    let table = if shared {
        "project_mailbox_receipts"
    } else {
        "message_recipients"
    };
    format!(
        "SELECT r.message_id, r.read_ts, r.ack_ts, \
         m.created_ts AS message_created_ts, m.subject AS message_subject, \
         p.id AS message_project_id, p.slug AS message_project_slug, \
         p.human_key AS message_project_human_key, \
         a.id AS agent_id, a.name AS agent_name, \
         ap.id AS project_id, ap.slug AS project_slug, ap.human_key AS project_human_key \
         FROM {table} r JOIN messages m ON m.id = r.message_id \
         JOIN projects p ON p.id = m.project_id JOIN agents a ON a.id = r.agent_id \
         JOIN projects ap ON ap.id = a.project_id"
    )
}

fn receipt(row: &Row, shared: bool) -> DbResult<ReceiptRecord> {
    Ok(ReceiptRecord {
        message_id: row
            .get_named("message_id")
            .map_err(|e| state_error("receipt", e))?,
        message_project: project(row, "message_")?,
        message_created_ts: row
            .get_named("message_created_ts")
            .map_err(|e| state_error("receipt", e))?,
        message_subject: row
            .get_named("message_subject")
            .map_err(|e| state_error("receipt", e))?,
        agent: agent(row, "")?,
        shared,
        read_ts: row
            .get_named("read_ts")
            .map_err(|e| state_error("receipt", e))?,
        ack_ts: row
            .get_named("ack_ts")
            .map_err(|e| state_error("receipt", e))?,
    })
}

const CONTACT_SELECT: &str = "SELECT l.id, l.status, l.reason, l.created_ts, l.updated_ts, l.expires_ts, \
     a.id AS from_agent_id, a.name AS from_agent_name, \
     ap.id AS from_project_id, ap.slug AS from_project_slug, ap.human_key AS from_project_human_key, \
     b.id AS to_agent_id, b.name AS to_agent_name, \
     bp.id AS to_project_id, bp.slug AS to_project_slug, bp.human_key AS to_project_human_key \
     FROM agent_links l JOIN agents a ON a.id = l.a_agent_id AND a.project_id = l.a_project_id \
     JOIN projects ap ON ap.id = a.project_id \
     JOIN agents b ON b.id = l.b_agent_id AND b.project_id = l.b_project_id \
     JOIN projects bp ON bp.id = b.project_id";

fn contact(row: &Row) -> DbResult<ContactRecord> {
    let status: String = row
        .get_named("status")
        .map_err(|e| state_error("contact", e))?;
    Ok(ContactRecord {
        id: row.get_named("id").map_err(|e| state_error("contact", e))?,
        from: agent(row, "from_")?,
        to: agent(row, "to_")?,
        status: status.trim().to_ascii_lowercase(),
        reason: row
            .get_named("reason")
            .map_err(|e| state_error("contact", e))?,
        created_ts: row
            .get_named("created_ts")
            .map_err(|e| state_error("contact", e))?,
        updated_ts: row
            .get_named("updated_ts")
            .map_err(|e| state_error("contact", e))?,
        expires_ts: row
            .get_named("expires_ts")
            .map_err(|e| state_error("contact", e))?,
    })
}

fn record_name(record: &RecoveryRecord) -> DbResult<String> {
    let (kind, identity) = match record {
        RecoveryRecord::Project(project) => ("project", serde_json::to_vec(project)),
        RecoveryRecord::Agent(agent) => ("agent", serde_json::to_vec(agent)),
        RecoveryRecord::Receipt(receipt) => (
            "receipt",
            serde_json::to_vec(&(
                receipt.message_id,
                &receipt.message_project,
                receipt.message_created_ts,
                &receipt.message_subject,
                &receipt.agent,
                receipt.shared,
            )),
        ),
        RecoveryRecord::Contact(contact) => (
            "contact",
            serde_json::to_vec(&(contact.id, &contact.from, &contact.to)),
        ),
    };
    let identity = identity.map_err(|error| state_error("record identity", error))?;
    Ok(format!(
        "{kind}-{}.json",
        hex::encode(Sha256::digest(identity))
    ))
}

fn validate_document(document: &RecordDocument, generation: &str) -> DbResult<()> {
    if document.schema_version != SCHEMA_VERSION || document.generation != generation {
        return Err(state_error(
            "record",
            "schema or database generation mismatch",
        ));
    }
    match &document.record {
        RecoveryRecord::Project(_) | RecoveryRecord::Agent(_) => {}
        RecoveryRecord::Receipt(record) => {
            if record.read_ts.is_none() && record.ack_ts.is_none() {
                return Err(state_error(
                    "receipt",
                    "empty receipt carries no recoverable state",
                ));
            }
        }
        RecoveryRecord::Contact(record) => {
            if !matches!(
                record.status.trim().to_ascii_lowercase().as_str(),
                "pending" | "approved" | "blocked"
            ) {
                return Err(state_error("contact", "unknown contact decision"));
            }
        }
    }
    Ok(())
}

fn encode_record(generation: &str, mut record: RecoveryRecord) -> DbResult<Vec<u8>> {
    if let RecoveryRecord::Contact(contact) = &mut record {
        contact.status = contact.status.trim().to_ascii_lowercase();
    }
    let document = RecordDocument {
        schema_version: SCHEMA_VERSION,
        generation: generation.to_string(),
        record,
    };
    validate_document(&document, generation)?;
    let mut bytes = serde_json::to_vec(&document).map_err(|e| state_error("encode", e))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(state_error("encode", "record exceeds bounded size limit"));
    }
    Ok(bytes)
}

fn validate_incremental_name(name: &str) -> DbResult<()> {
    let digest = name
        .strip_prefix("receipt-")
        .or_else(|| name.strip_prefix("contact-"))
        .and_then(|suffix| suffix.strip_suffix(".json"));
    if !digest.is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(state_error(
            "record identity",
            "invalid incremental record filename",
        ));
    }
    Ok(())
}

fn incremental_digest(name: &str, bytes: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"agent-mail-recovery-incremental-v1\0");
    digest.update(name.as_bytes());
    digest.update([0]);
    digest.update(bytes);
    digest.finalize().into()
}

fn read_incremental_record(
    directory: &JournalDirectory,
    name: &str,
    generation: &str,
) -> DbResult<Option<(RecordDocument, [u8; 32])>> {
    validate_incremental_name(name)?;
    let Some(bytes) = read_bytes_bounded(directory, name, MAX_RECORD_BYTES)? else {
        return Ok(None);
    };
    let document: RecordDocument =
        serde_json::from_slice(&bytes).map_err(|error| state_error("record decode", error))?;
    validate_document(&document, generation)?;
    if record_name(&document.record)? != name {
        return Err(state_error(
            "record identity",
            "record does not match its filename",
        ));
    }
    Ok(Some((document, incremental_digest(name, &bytes))))
}

fn verify_incremental_inventory(
    directory: &JournalDirectory,
    expected: &IncrementalInventory,
) -> DbResult<()> {
    let actual = visit_incremental_records(directory, &expected.generation, &mut |_| Ok(()))?;
    if actual != *expected {
        return Err(state_error(
            "incremental inventory",
            "record count or contents differ from the sealed publication",
        ));
    }
    Ok(())
}

/// Preserve the last expected set when resuming an interrupted writer. Only its
/// last write-ahead record can be in either state; all other keys must still
/// match the previous seal. Reconciliation must never bless a partial copy.
fn publication_inventory(
    state: &LockedState,
    active: Option<&Manifest>,
    generation: &str,
    previous: Option<&ReadRevision>,
) -> DbResult<IncrementalInventory> {
    let Some(active) = active else {
        if previous.is_some_and(|revision| {
            revision.pending_record.is_some()
                || revision
                    .inventory
                    .as_ref()
                    .is_some_and(|inventory| inventory.count != 0)
        }) {
            return Err(state_error(
                "incremental inventory",
                "incremental authority exists without its active checkpoint",
            ));
        }
        return Ok(IncrementalInventory::empty(generation));
    };
    let directory = state.generation(&active.generation, false)?;
    let Some(previous) = previous.filter(|revision| revision.inventory.is_some()) else {
        // Version 1 recorded no per-key completeness evidence. Import its
        // existing valid files, then reconcile the live database before sealing.
        return visit_incremental_records(&directory, &active.generation, &mut |_| Ok(()));
    };
    let mut inventory = previous
        .inventory
        .clone()
        .ok_or_else(|| state_error("incremental inventory", "previous seal is missing"))?;
    if inventory.generation != active.generation {
        // A switch can publish its new baseline before the pending revision is
        // advanced. No incremental write is admitted until that advancement.
        if previous.manifest.is_some()
            || previous.pending_record.is_some()
            || previous.generation != active.generation
        {
            return Err(state_error(
                "incremental inventory",
                "pending inventory does not cover the active generation",
            ));
        }
        inventory = IncrementalInventory::empty(&active.generation);
        verify_incremental_inventory(&directory, &inventory)?;
        return Ok(inventory);
    }
    if let Some(pending) = &previous.pending_record {
        let current = read_incremental_record(&directory, &pending.name, &inventory.generation)?
            .map(|(_, digest)| digest);
        if current.as_ref() == Some(&pending.after) {
            inventory.replace(pending.before.as_ref(), &pending.after)?;
        } else if current != pending.before {
            return Err(state_error(
                "incremental inventory",
                "interrupted record matches neither its before nor after contents",
            ));
        }
    }
    if previous.manifest.is_none() {
        verify_incremental_inventory(&directory, &inventory)?;
    }
    Ok(inventory)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitPendingEntry {
    schema_version: u32,
    /// A missing path requests one complete inventory after upgrading an older
    /// archive or checking it out without its runtime-only publication journal.
    path: Option<String>,
}

fn ensure_git_pending_journal(state: &LockedState) -> DbResult<()> {
    match state
        .directory
        .open_file(GIT_PENDING_FILE, JournalFileMode::Read)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut bytes = serde_json::to_vec(&GitPendingEntry {
                schema_version: SCHEMA_VERSION,
                path: None,
            })
            .map_err(|error| state_error("Git pending initialization", error))?;
            bytes.push(b'\n');
            state
                .directory
                .atomic_write(GIT_PENDING_FILE, |file| file.write_all(&bytes))
                .map_err(|error| state_error("Git pending initialization", error))
        }
        Err(error) => Err(state_error("Git pending open", error)),
    }
}

/// Return the artifact filename and its optional database generation. Runtime
/// locks, temporary files and the outbox itself are never publication inputs.
fn git_artifact_parts(path: &str) -> DbResult<(&str, Option<&str>)> {
    let invalid = || state_error("Git publication", "path is not a recovery artifact");
    let mut parts = path.split('/');
    if parts.next() != Some(DIRECTORY) {
        return Err(invalid());
    }
    let first = parts.next().ok_or_else(invalid)?;
    let Some(name) = parts.next() else {
        return if matches!(
            first,
            MANIFEST_FILE | READ_REVISION_FILE | "delivery-cursor.json"
        ) {
            Ok((first, None))
        } else {
            Err(invalid())
        };
    };
    if parts.next().is_some() {
        return Err(invalid());
    }
    let generation = first.strip_prefix('g').ok_or_else(invalid)?;
    generation_directory(generation)?;
    let record_digest = name
        .strip_prefix("receipt-")
        .or_else(|| name.strip_prefix("contact-"))
        .and_then(|name| name.strip_suffix(".json"));
    if !matches!(name, BASELINE_FILE | SUPERSEDED_FILE)
        && !record_digest.is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
    {
        return Err(invalid());
    }
    Ok((name, Some(generation)))
}

/// The path debt must reach durable storage before its artifact can change.
/// A later publisher in any process can then include every changed key with
/// the current completed revision, even if the producer exits before enqueue.
fn append_git_pending_path(state: &LockedState, path: &str) -> DbResult<()> {
    git_artifact_parts(path)?;
    ensure_git_pending_journal(state)?;
    let mut bytes = serde_json::to_vec(&GitPendingEntry {
        schema_version: SCHEMA_VERSION,
        path: Some(path.to_string()),
    })
    .map_err(|error| state_error("Git pending encode", error))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_GIT_PENDING_RECORD_BYTES {
        return Err(state_error(
            "Git pending append",
            "path exceeds bounded record size",
        ));
    }
    let mut file = state
        .directory
        .open_file(GIT_PENDING_FILE, JournalFileMode::Append)
        .map_err(|error| state_error("Git pending append", error))?;
    let length = file
        .metadata()
        .map_err(|error| state_error("Git pending metadata", error))?
        .len();
    if length > MAX_GIT_PENDING_BYTES.saturating_sub(bytes.len() as u64) {
        return Err(state_error(
            "Git pending append",
            "publication debt exceeds bounded size",
        ));
    }
    if length > 0 {
        file.seek(io::SeekFrom::End(-1))
            .map_err(|error| state_error("Git pending tail", error))?;
        let mut last = [0u8];
        file.read_exact(&mut last)
            .map_err(|error| state_error("Git pending tail", error))?;
        if last != *b"\n" {
            return Err(state_error(
                "Git pending tail",
                "incomplete publication record",
            ));
        }
    }
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| state_error("Git pending append", error))?;
    state
        .directory
        .sync()
        .map_err(|error| state_error("Git pending sync", error))?;
    state
        .directory
        .validate_file(GIT_PENDING_FILE, &file)
        .map_err(|error| state_error("Git pending binding", error))
}

#[derive(Default)]
struct GitPublicationPaths {
    paths: BTreeSet<String>,
    bytes: u64,
}

impl GitPublicationPaths {
    fn insert(&mut self, path: String) -> DbResult<()> {
        git_artifact_parts(&path)?;
        if self.paths.contains(&path) {
            return Ok(());
        }
        self.bytes = self.bytes.saturating_add(path.len() as u64);
        if self.bytes > MAX_GIT_PENDING_BYTES {
            return Err(state_error(
                "Git publication inventory",
                "paths exceed bounded size",
            ));
        }
        self.paths.insert(path);
        Ok(())
    }
}

fn collect_git_pending_paths(
    state: &LockedState,
    paths: &mut GitPublicationPaths,
) -> DbResult<bool> {
    let file = state
        .directory
        .open_file(GIT_PENDING_FILE, JournalFileMode::Read)
        .map_err(|error| state_error("Git pending open", error))?;
    let length = file
        .metadata()
        .map_err(|error| state_error("Git pending metadata", error))?
        .len();
    if length > MAX_GIT_PENDING_BYTES {
        return Err(state_error(
            "Git pending read",
            "publication debt exceeds bounded size",
        ));
    }
    let mut reader = io::BufReader::new(&file);
    let mut consumed = 0u64;
    let mut inventory = false;
    loop {
        let mut bytes = Vec::new();
        let count = reader
            .by_ref()
            .take(MAX_GIT_PENDING_RECORD_BYTES + 1)
            .read_until(b'\n', &mut bytes)
            .map_err(|error| state_error("Git pending read", error))?;
        if count == 0 {
            break;
        }
        consumed = consumed.saturating_add(count as u64);
        if consumed > MAX_GIT_PENDING_BYTES
            || count as u64 > MAX_GIT_PENDING_RECORD_BYTES
            || bytes.last() != Some(&b'\n')
        {
            return Err(state_error(
                "Git pending read",
                "oversized or incomplete publication record",
            ));
        }
        let entry: GitPendingEntry = serde_json::from_slice(&bytes)
            .map_err(|error| state_error("Git pending decode", error))?;
        if entry.schema_version != SCHEMA_VERSION {
            return Err(state_error(
                "Git pending decode",
                "unsupported schema version",
            ));
        }
        if let Some(path) = entry.path {
            paths.insert(path)?;
        } else {
            inventory = true;
        }
    }
    if consumed != length
        || file
            .metadata()
            .map_err(|error| state_error("Git pending metadata", error))?
            .len()
            != length
    {
        return Err(state_error(
            "Git pending read",
            "publication debt changed during inspection",
        ));
    }
    state
        .directory
        .validate_file(GIT_PENDING_FILE, &file)
        .map_err(|error| state_error("Git pending binding", error))?;
    Ok(inventory)
}

/// Legacy archives and fresh Git checkouts have no outbox. Inventory their
/// active generation once; later publications inspect only accumulated debt.
fn collect_git_generation_inventory(
    state: &LockedState,
    manifest: Option<&Manifest>,
    paths: &mut GitPublicationPaths,
) -> DbResult<()> {
    if let Some(manifest) = manifest {
        let directory = state.generation(&manifest.generation, false)?;
        for entry in std::fs::read_dir(directory.directory_path())
            .map_err(|error| state_error("Git generation inventory", error))?
        {
            let name = entry
                .map_err(|error| state_error("Git generation inventory", error))?
                .file_name();
            let Some(name) = name.to_str() else {
                return Err(state_error(
                    "Git generation inventory",
                    "non-UTF-8 recovery filename",
                ));
            };
            if name.starts_with("receipt-") || name.starts_with("contact-") {
                paths.insert(format!(
                    "{DIRECTORY}/{}/{name}",
                    generation_directory(&manifest.generation)?
                ))?;
            }
        }
    }
    // Retain supersession witnesses too: a restored old database must not
    // reactivate an already abandoned archive generation after a Git checkout.
    for entry in std::fs::read_dir(state.directory.directory_path())
        .map_err(|error| state_error("Git generation inventory", error))?
    {
        let name = entry
            .map_err(|error| state_error("Git generation inventory", error))?
            .file_name();
        let Some(generation) = name.to_str().and_then(|name| name.strip_prefix('g')) else {
            continue;
        };
        let directory = state.generation(generation, false)?;
        if let Some(manifest) = read_json::<Manifest>(&directory, SUPERSEDED_FILE)? {
            validate_manifest(&manifest)?;
            if manifest.generation != generation {
                return Err(state_error(
                    "Git generation inventory",
                    "superseded generation identity mismatch",
                ));
            }
            paths.insert(format!(
                "{DIRECTORY}/{}/{SUPERSEDED_FILE}",
                generation_directory(generation)?
            ))?;
        }
    }
    Ok(())
}

fn write_record(
    state: &LockedState,
    root: &Path,
    directory: &JournalDirectory,
    generation: &str,
    revision: &mut ReadRevision,
    mut record: RecoveryRecord,
) -> DbResult<()> {
    let name = record_name(&record)?;
    let previous = read_incremental_record(directory, &name, generation)?;
    if let Some((previous, _)) = &previous {
        if let (RecoveryRecord::Receipt(current), RecoveryRecord::Receipt(old)) =
            (&mut record, &previous.record)
        {
            current.read_ts = current.read_ts.into_iter().chain(old.read_ts).min();
            current.ack_ts = current.ack_ts.into_iter().chain(old.ack_ts).min();
        }
        if record == previous.record {
            return Ok(());
        }
    }
    let bytes = encode_record(generation, record)?;
    let before = previous.as_ref().map(|(_, digest)| *digest);
    let after = incremental_digest(&name, &bytes);
    let mut inventory = revision
        .inventory
        .clone()
        .ok_or_else(|| state_error("record publication", "incremental inventory is missing"))?;
    if inventory.generation != generation
        || revision.manifest.is_some()
        || revision.pending_record.is_some()
    {
        return Err(state_error(
            "record publication",
            "record does not belong to an admitted pending publication",
        ));
    }
    inventory.replace(before.as_ref(), &after)?;
    let relative = format!("{DIRECTORY}/{}/{name}", generation_directory(generation)?);
    append_git_pending_path(state, &relative)?;
    // Preserve the old seal plus one exact before/after transition before
    // replacing the file. A crash can leave either version, but never permits
    // silently recomputing a seal from whatever subset happened to survive.
    revision.pending_record = Some(PendingRecord {
        name: name.clone(),
        before,
        after,
    });
    write_revision(state, revision)?;
    #[cfg(test)]
    {
        let hook = AFTER_RECORD_INTENT_TEST_HOOK.with(|hook| hook.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }
    directory
        .atomic_write(&name, |file| file.write_all(&bytes))
        .map_err(|error| state_error("record publication", error))?;
    #[cfg(test)]
    {
        let hook = AFTER_RECORD_REPLACE_TEST_HOOK.with(|hook| hook.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }
    revision.inventory = Some(inventory);
    revision.pending_record = None;
    note_pending_archive_paths(root, [relative]);
    Ok(())
}

struct BaselineWriter<'a> {
    file: &'a mut File,
    manifest: Manifest,
    digest: Sha256,
}

impl BaselineWriter<'_> {
    fn push(&mut self, record: RecoveryRecord) -> DbResult<()> {
        match &record {
            RecoveryRecord::Project(_) => self.manifest.projects += 1,
            RecoveryRecord::Agent(_) => self.manifest.agents += 1,
            RecoveryRecord::Receipt(_) => self.manifest.receipts += 1,
            RecoveryRecord::Contact(_) => self.manifest.contacts += 1,
        }
        let bytes = encode_record(&self.manifest.generation, record)?;
        self.file
            .write_all(&bytes)
            .map_err(|error| state_error("baseline write", error))?;
        self.digest.update(&bytes);
        Ok(())
    }
}

fn checkpoint_identities(conn: &impl SyncQuery, writer: &mut BaselineWriter<'_>) -> DbResult<()> {
    let mut last_id = i64::MIN;
    loop {
        let rows = query(
            conn,
            &format!(
                "SELECT id AS project_id, slug AS project_slug, human_key AS project_human_key \
             FROM projects WHERE id > ? ORDER BY id LIMIT {BATCH_ROWS}"
            ),
            &[Value::BigInt(last_id)],
        )?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            let identity = project(row, "")?;
            last_id = identity.id;
            writer.push(RecoveryRecord::Project(identity))?;
        }
    }
    last_id = i64::MIN;
    loop {
        let rows = query(
            conn,
            &format!(
                "SELECT a.id AS agent_id, a.name AS agent_name, \
             p.id AS project_id, p.slug AS project_slug, p.human_key AS project_human_key \
             FROM agents a JOIN projects p ON p.id = a.project_id \
             WHERE a.id > ? ORDER BY a.id LIMIT {BATCH_ROWS}"
            ),
            &[Value::BigInt(last_id)],
        )?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            let identity = agent(row, "")?;
            last_id = identity.id;
            writer.push(RecoveryRecord::Agent(identity))?;
        }
    }
    Ok(())
}

fn visit_current_receipts(
    conn: &impl SyncQuery,
    visit: &mut impl FnMut(RecoveryRecord) -> DbResult<()>,
) -> DbResult<()> {
    for shared in [false, true] {
        let mut last_agent = i64::MIN;
        let mut last_message = i64::MIN;
        loop {
            let sql = format!(
                "{} WHERE (r.agent_id > ? OR (r.agent_id = ? AND r.message_id > ?)) \
                 AND (r.read_ts IS NOT NULL OR r.ack_ts IS NOT NULL) \
                 ORDER BY r.agent_id, r.message_id LIMIT {BATCH_ROWS}",
                receipt_select(shared),
            );
            let rows = query(
                conn,
                &sql,
                &[
                    Value::BigInt(last_agent),
                    Value::BigInt(last_agent),
                    Value::BigInt(last_message),
                ],
            )?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                let receipt = receipt(row, shared)?;
                last_agent = receipt.agent.id;
                last_message = receipt.message_id;
                visit(RecoveryRecord::Receipt(receipt))?;
            }
        }
    }
    Ok(())
}

fn visit_current_contacts(
    conn: &impl SyncQuery,
    visit: &mut impl FnMut(RecoveryRecord) -> DbResult<()>,
) -> DbResult<()> {
    let mut last_id = i64::MIN;
    loop {
        let rows = query(
            conn,
            &format!("{CONTACT_SELECT} WHERE l.id > ? ORDER BY l.id LIMIT {BATCH_ROWS}"),
            &[Value::BigInt(last_id)],
        )?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            let contact = contact(row)?;
            last_id = contact.id;
            visit(RecoveryRecord::Contact(contact))?;
        }
    }
    Ok(())
}

fn verify_checkpoint_counts(conn: &impl SyncQuery, manifest: &Manifest) -> DbResult<()> {
    verify_recovery_counts(
        conn,
        &[
            ("projects", manifest.projects),
            ("agents", manifest.agents),
            ("receipts", manifest.receipts),
            ("contacts", manifest.contacts),
        ],
    )
}

fn verify_recovery_counts(conn: &impl SyncQuery, written_counts: &[(&str, u64)]) -> DbResult<()> {
    let counts = query(
        conn,
        "SELECT (SELECT COUNT(*) FROM projects) AS projects, \
         (SELECT COUNT(*) FROM agents) AS agents, \
         (SELECT COUNT(*) FROM message_recipients WHERE read_ts IS NOT NULL OR ack_ts IS NOT NULL) \
           + (SELECT COUNT(*) FROM project_mailbox_receipts WHERE read_ts IS NOT NULL OR ack_ts IS NOT NULL) AS receipts, \
         (SELECT COUNT(*) FROM agent_links) AS contacts",
        &[],
    )?;
    let counts = counts
        .first()
        .ok_or_else(|| state_error("baseline counts", "missing count row"))?;
    for &(column, written) in written_counts {
        let actual: i64 = counts
            .get_named(column)
            .map_err(|error| state_error("baseline counts", error))?;
        if u64::try_from(actual).ok() != Some(written) {
            return Err(state_error(
                "baseline counts",
                format!(
                    "{column}: wrote {written} of {actual}; refusing incomplete identity joins"
                ),
            ));
        }
    }
    Ok(())
}

fn stream_checkpoint(
    conn: &impl SyncQuery,
    file: &mut File,
    generation: &str,
) -> DbResult<Manifest> {
    let mut writer = BaselineWriter {
        file,
        manifest: Manifest {
            schema_version: SCHEMA_VERSION,
            generation: generation.to_string(),
            baseline_sha256: String::new(),
            projects: 0,
            agents: 0,
            receipts: 0,
            contacts: 0,
        },
        digest: Sha256::new(),
    };
    checkpoint_identities(conn, &mut writer)?;
    visit_current_receipts(conn, &mut |record| writer.push(record))?;
    visit_current_contacts(conn, &mut |record| writer.push(record))?;
    verify_checkpoint_counts(conn, &writer.manifest)?;
    writer.manifest.baseline_sha256 = hex::encode(writer.digest.finalize());
    Ok(writer.manifest)
}

fn publish_checkpoint(
    conn: &impl SyncQuery,
    directory: &JournalDirectory,
    generation: &str,
) -> DbResult<Manifest> {
    crate::sync::with_read_snapshot(conn, || {
        let mut checkpoint = None;
        let publication = directory.atomic_write(BASELINE_FILE, |file| {
            checkpoint = Some(stream_checkpoint(conn, file, generation));
            match checkpoint.as_ref() {
                Some(Ok(_)) => Ok(()),
                Some(Err(error)) => Err(io::Error::other(error.to_string())),
                None => Err(io::Error::other("checkpoint writer did not run")),
            }
        });
        let manifest = checkpoint.ok_or_else(|| {
            state_error("baseline publication", "checkpoint writer was not admitted")
        })??;
        publication.map_err(|error| state_error("baseline publication", error))?;
        Ok(manifest)
    })
    .map_err(|error| state_error("baseline snapshot", error))
}

fn validate_checkpoint_generation(
    state: &LockedState,
    generation: &str,
    active: Option<&Manifest>,
) -> DbResult<()> {
    if active.is_some_and(|manifest| manifest.generation == generation) {
        return Ok(());
    }
    state.validate()?;
    let directory = match JournalDirectory::open(
        &state.directory.directory_path(),
        &generation_directory(generation)?,
        false,
    ) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(state_error("generation directory", error)),
    };
    if read_json::<Manifest>(&directory, SUPERSEDED_FILE)?.is_some() {
        return Err(state_error(
            "checkpoint",
            "refusing to reactivate a superseded database generation; restore the matching archive checkpoint with this database backup, or reconstruct a fresh database from the active archive",
        ));
    }
    for entry in std::fs::read_dir(directory.directory_path())
        .map_err(|error| state_error("generation inventory", error))?
    {
        let name = entry
            .map_err(|error| state_error("generation inventory", error))?
            .file_name();
        let name = name.to_string_lossy();
        if name.starts_with("receipt-") || name.starts_with("contact-") {
            return Err(state_error(
                "checkpoint",
                "refusing to reactivate an abandoned generation with incremental records",
            ));
        }
    }
    Ok(())
}

fn checkpoint_locked(
    state: &LockedState,
    conn: &impl SyncQuery,
    root: &Path,
    generation: &str,
) -> DbResult<Manifest> {
    let active = read_manifest(&state.directory)?;
    if let Some(manifest) = active.as_ref()
        && manifest.generation == generation
    {
        // The baseline was atomically completed before this marker. Hot writes
        // do not scan mailbox history or fsync an unchanged checkpoint.
        state
            .generation(generation, false)?
            .open_file(BASELINE_FILE, JournalFileMode::Read)
            .map_err(|error| state_error("active baseline", error))?;
        return Ok(manifest.clone());
    }
    validate_checkpoint_generation(state, generation, active.as_ref())?;
    let directory = state.generation(generation, true)?;
    if let Some(previous) = active {
        // Record previous activation before changing active.json. An orphan
        // baseline created by an interrupted first publication has no such
        // marker and can safely be regenerated. The old active generation
        // remains usable if the attempted switch fails before publication.
        let prior_directory = state.generation(&previous.generation, false)?;
        let bytes = serde_json::to_vec(&previous)
            .map_err(|error| state_error("superseded manifest encode", error))?;
        let superseded_path = format!(
            "{DIRECTORY}/{}/{SUPERSEDED_FILE}",
            generation_directory(&previous.generation)?
        );
        append_git_pending_path(state, &superseded_path)?;
        prior_directory
            .atomic_write(SUPERSEDED_FILE, |file| file.write_all(&bytes))
            .map_err(|error| state_error("superseded generation publication", error))?;
        note_pending_archive_paths(root, [superseded_path]);
    }
    append_git_pending_path(state, &format!("{DIRECTORY}/{MANIFEST_FILE}"))?;
    append_git_pending_path(
        state,
        &format!(
            "{DIRECTORY}/{}/{BASELINE_FILE}",
            generation_directory(generation)?
        ),
    )?;
    let manifest = publish_checkpoint(conn, &directory, generation)?;
    state.validate()?;
    let bytes =
        serde_json::to_vec(&manifest).map_err(|error| state_error("manifest encode", error))?;
    state
        .directory
        .atomic_write(MANIFEST_FILE, |file| file.write_all(&bytes))
        .map_err(|error| state_error("manifest publication", error))?;
    state.validate()?;
    note_pending_archive_paths(
        root,
        [
            format!("{DIRECTORY}/{MANIFEST_FILE}"),
            format!(
                "{DIRECTORY}/{}/{BASELINE_FILE}",
                generation_directory(generation)?
            ),
        ],
    );
    Ok(manifest)
}

/// Complete interrupted publications from one live database snapshot before
/// any later writer can publish a reusable, authoritative revision.
fn reconcile_incomplete_publication(
    state: &LockedState,
    conn: &impl SyncQuery,
    root: &Path,
    generation: &str,
    revision: &mut ReadRevision,
) -> DbResult<()> {
    checkpoint_locked(state, conn, root, generation)?;
    let directory = state.generation(generation, false)?;
    crate::sync::with_read_snapshot(conn, || {
        let mut receipts = 0u64;
        visit_current_receipts(conn, &mut |record| {
            write_record(state, root, &directory, generation, revision, record)?;
            receipts += 1;
            Ok(())
        })?;
        let mut contacts = 0u64;
        visit_current_contacts(conn, &mut |record| {
            write_record(state, root, &directory, generation, revision, record)?;
            contacts += 1;
            Ok(())
        })?;
        // The baseline may predate new projects/agents. Check the two mutable
        // state lanes against their raw table counts so missing identity joins
        // cannot turn a partial refresh into completed publication authority.
        verify_recovery_counts(conn, &[("receipts", receipts), ("contacts", contacts)])
    })
    .map_err(|error| state_error("incomplete publication reconciliation", error))?;
    state.validate()
}

/// Publish the initial consistent baseline unless this generation already has
/// one. The generation's incremental snapshots remain authoritative thereafter.
pub fn ensure_checkpoint(conn: &impl SyncQuery, root: &Path) -> DbResult<Manifest> {
    let state = LockedState::open(root, true)?;
    let generation = ensure_generation(conn)?;
    with_read_revision(&state, conn, root, &generation, |revision| {
        let manifest = checkpoint_locked(&state, conn, root, &generation)?;
        // Startup verifies the existing seal too. Healthy selected-key writers
        // carry the expected inventory without rescanning mailbox history.
        let Some((observed, inventory)) = visit_records_with_inventory(root, |_| Ok(()))? else {
            return Err(state_error(
                "startup checkpoint",
                "completed generation disappeared during validation",
            ));
        };
        if observed != manifest || revision.inventory.as_ref() != Some(&inventory) {
            return Err(state_error(
                "startup checkpoint",
                "incremental inventory or checkpoint differs from the sealed publication",
            ));
        }
        queue_superseded_generations(&state, root)?;
        let directory = state.generation(&generation, false)?;
        note_pending_archive_paths(
            root,
            [
                format!("{DIRECTORY}/{MANIFEST_FILE}"),
                format!(
                    "{DIRECTORY}/{}/{BASELINE_FILE}",
                    generation_directory(&generation)?
                ),
            ],
        );
        for entry in std::fs::read_dir(directory.directory_path())
            .map_err(|error| state_error("publication inventory", error))?
        {
            let name = entry
                .map_err(|error| state_error("publication inventory", error))?
                .file_name();
            let Some(name) = name.to_str() else {
                return Err(state_error(
                    "publication inventory",
                    "non-UTF-8 recovery filename",
                ));
            };
            // Recovery filenames are a case-sensitive on-disk schema. A
            // differently cased extension is not a canonical record.
            #[allow(clippy::case_sensitive_file_extension_comparisons)]
            if (name.starts_with("receipt-") || name.starts_with("contact-"))
                && name.ends_with(".json")
            {
                note_pending_archive_paths(
                    root,
                    [format!(
                        "{DIRECTORY}/{}/{name}",
                        generation_directory(&generation)?
                    )],
                );
            }
        }
        state.validate()?;
        Ok(manifest)
    })
}

fn queue_superseded_generations(state: &LockedState, root: &Path) -> DbResult<()> {
    for entry in std::fs::read_dir(state.directory.directory_path())
        .map_err(|error| state_error("generation publication inventory", error))?
    {
        let entry =
            entry.map_err(|error| state_error("generation publication inventory", error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(state_error(
                "generation publication inventory",
                "non-UTF-8 recovery filename",
            ));
        };
        let Some(generation) = name.strip_prefix('g') else {
            continue;
        };
        let directory = state.generation(generation, false)?;
        if let Some(manifest) = read_json::<Manifest>(&directory, SUPERSEDED_FILE)? {
            validate_manifest(&manifest)?;
            if manifest.generation != generation {
                return Err(state_error(
                    "superseded generation",
                    "marker identity differs from its directory",
                ));
            }
            note_pending_archive_paths(root, [format!("{DIRECTORY}/{name}/{SUPERSEDED_FILE}")]);
        }
    }
    Ok(())
}

/// Checkpoint all currently recoverable state on first initialization.
pub fn checkpoint_all(conn: &impl SyncQuery, root: &Path) -> DbResult<Manifest> {
    ensure_checkpoint(conn, root)
}

/// Bind a Git commit's completed witness to every changed recovery artifact.
///
/// The storage publisher holds its own archive locks outside this function;
/// state writers append durable path debt before changing files and never call
/// back into Git while locked. The caller's pending paths are only process-local
/// wakeup hints: they can predate the latest revision or omit another process.
pub fn with_archive_publication<T>(
    root: &Path,
    pending: &[String],
    publish: impl FnOnce(&[String]) -> DbResult<T>,
) -> DbResult<T> {
    let state = LockedState::open(root, false)?;
    ensure_git_pending_journal(&state)?;
    let mut paths = GitPublicationPaths::default();
    for path in pending {
        paths.insert(path.clone())?;
    }
    let inventory = collect_git_pending_paths(&state, &mut paths)?;
    let manifest = read_manifest(&state.directory)?;
    if inventory {
        collect_git_generation_inventory(&state, manifest.as_ref(), &mut paths)?;
    }
    if let Some(manifest) = &manifest {
        let generation = state.generation(&manifest.generation, false)?;
        generation
            .open_file(BASELINE_FILE, JournalFileMode::Read)
            .map_err(|error| state_error("Git baseline", error))?;
        paths.insert(format!("{DIRECTORY}/{MANIFEST_FILE}"))?;
        paths.insert(format!(
            "{DIRECTORY}/{}/{BASELINE_FILE}",
            generation_directory(&manifest.generation)?
        ))?;
    }
    if let Some(revision) = read_revision(&state.directory)? {
        if revision
            .manifest
            .as_ref()
            .is_some_and(|expected| manifest.as_ref() != Some(expected))
        {
            return Err(state_error(
                "Git publication",
                "completed revision differs from its active manifest",
            ));
        }
        paths.insert(format!("{DIRECTORY}/{READ_REVISION_FILE}"))?;
    }
    // Cursors have a separate lock and a monotonic ceiling, so include their
    // current file even when the originating process's hint was lost.
    match state
        .directory
        .open_file("delivery-cursor.json", JournalFileMode::Read)
    {
        Ok(_) => paths.insert(format!("{DIRECTORY}/delivery-cursor.json"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(state_error("Git cursor", error)),
    }
    // A write-ahead entry can survive a failed artifact write. Never let Git's
    // missing-path behavior turn that debt into removal of a committed file.
    let mut publication = Vec::with_capacity(paths.paths.len());
    for path in paths.paths {
        let (name, generation) = git_artifact_parts(&path)?;
        let file = if let Some(generation) = generation {
            state
                .generation(generation, false)?
                .open_file(name, JournalFileMode::Read)
        } else {
            state.directory.open_file(name, JournalFileMode::Read)
        };
        if let Err(error) = file {
            // An interrupted generation switch can leave intent for a baseline
            // that never became active, or for superseding the still-active
            // generation. Neither absent file is current recovery authority.
            // Skip them without touching their existing Git entries, if any.
            let abandoned_switch = manifest.as_ref().is_some_and(|active| {
                generation.is_some_and(|generation| {
                    (name == BASELINE_FILE && generation != active.generation.as_str())
                        || (name == SUPERSEDED_FILE && generation == active.generation.as_str())
                })
            });
            if error.kind() == io::ErrorKind::NotFound && abandoned_switch {
                continue;
            }
            return Err(state_error("Git artifact", error));
        }
        publication.push(path);
    }
    let result = publish(&publication)?;
    state.validate()?;
    // On an error or unwind the old debt is untouched. A crash after Git's
    // commit but before this compaction merely republishes the same paths.
    state
        .directory
        .atomic_write(GIT_PENDING_FILE, |_| Ok(()))
        .map_err(|error| state_error("Git pending completion", error))?;
    state.validate()?;
    Ok(result)
}

/// Persist actual committed direct and shared receipts for the selected agent.
/// Missing/non-recipient message IDs never fabricate a recovery receipt.
pub fn persist_receipts(
    conn: &impl SyncQuery,
    root: &Path,
    agent_id: i64,
    message_ids: &[i64],
) -> DbResult<()> {
    if message_ids.is_empty() {
        return Ok(());
    }
    let state = LockedState::open(root, true)?;
    let generation = ensure_generation(conn)?;
    with_read_revision(&state, conn, root, &generation, |revision| {
        checkpoint_locked(&state, conn, root, &generation)?;
        let directory = state.generation(&generation, true)?;
        crate::sync::with_read_snapshot(conn, || {
            for shared in [false, true] {
                for ids in message_ids.chunks(BATCH_ROWS) {
                    let placeholders = std::iter::repeat_n("?", ids.len())
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql = format!(
                        "{} WHERE r.agent_id = ? AND r.message_id IN ({placeholders}) \
                     AND (r.read_ts IS NOT NULL OR r.ack_ts IS NOT NULL)",
                        receipt_select(shared),
                    );
                    let mut params = vec![Value::BigInt(agent_id)];
                    params.extend(ids.iter().copied().map(Value::BigInt));
                    for row in query(conn, &sql, &params)? {
                        write_record(
                            &state,
                            root,
                            &directory,
                            &generation,
                            revision,
                            RecoveryRecord::Receipt(receipt(&row, shared)?),
                        )?;
                    }
                }
            }
            Ok(())
        })
        .map_err(|error| state_error("receipt snapshot", error))?;
        state.validate()
    })
}

/// Reconcile all committed receipts for one project's bulk mark-read operation.
///
/// A retry may select no unread rows after an earlier database commit whose
/// archive publication failed. Paging the settled receipts also completes that
/// earlier publication without retaining an unbounded in-memory retry queue.
pub fn persist_project_receipts(
    conn: &impl SyncQuery,
    root: &Path,
    project_id: i64,
    agent_id: i64,
) -> DbResult<()> {
    let state = LockedState::open(root, true)?;
    let generation = ensure_generation(conn)?;
    with_read_revision(&state, conn, root, &generation, |revision| {
        checkpoint_locked(&state, conn, root, &generation)?;
        let directory = state.generation(&generation, true)?;
        crate::sync::with_read_snapshot(conn, || {
            for shared in [false, true] {
                let mut last_message = i64::MIN;
                loop {
                    let sql = format!(
                        "{} WHERE r.agent_id = ? AND m.project_id = ? AND r.message_id > ? \
                     AND (r.read_ts IS NOT NULL OR r.ack_ts IS NOT NULL) \
                     ORDER BY r.message_id LIMIT {BATCH_ROWS}",
                        receipt_select(shared),
                    );
                    let rows = query(
                        conn,
                        &sql,
                        &[
                            Value::BigInt(agent_id),
                            Value::BigInt(project_id),
                            Value::BigInt(last_message),
                        ],
                    )?;
                    if rows.is_empty() {
                        break;
                    }
                    for row in &rows {
                        let receipt = receipt(row, shared)?;
                        last_message = receipt.message_id;
                        write_record(
                            &state,
                            root,
                            &directory,
                            &generation,
                            revision,
                            RecoveryRecord::Receipt(receipt),
                        )?;
                    }
                }
            }
            Ok(())
        })
        .map_err(|error| state_error("bulk receipt snapshot", error))?;
        state.validate()
    })
}

/// Publish the current contact decision, reread while holding the publication
/// lock. The timestamps in a delayed caller's earlier return value are not used
/// to order approvals and blocks.
pub fn persist_contact(
    conn: &impl SyncQuery,
    root: &Path,
    from_agent_id: i64,
    to_agent_id: i64,
) -> DbResult<()> {
    let state = LockedState::open(root, true)?;
    let generation = ensure_generation(conn)?;
    with_read_revision(&state, conn, root, &generation, |revision| {
        checkpoint_locked(&state, conn, root, &generation)?;
        let directory = state.generation(&generation, true)?;
        crate::sync::with_read_snapshot(conn, || {
            let rows = query(
                conn,
                &format!("{CONTACT_SELECT} WHERE l.a_agent_id = ? AND l.b_agent_id = ?"),
                &[Value::BigInt(from_agent_id), Value::BigInt(to_agent_id)],
            )?;
            if rows.is_empty() {
                return Err(state_error(
                    "contact publication",
                    "committed contact row is absent",
                ));
            }
            for row in &rows {
                write_record(
                    &state,
                    root,
                    &directory,
                    &generation,
                    revision,
                    RecoveryRecord::Contact(contact(row)?),
                )?;
            }
            Ok(())
        })
        .map_err(|error| state_error("contact snapshot", error))?;
        state.validate()
    })
}

fn checked_baseline(generation: &JournalDirectory, manifest: &Manifest) -> DbResult<File> {
    let mut baseline = generation
        .open_file(BASELINE_FILE, JournalFileMode::Read)
        .map_err(|error| state_error("baseline open", error))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024].into_boxed_slice();
    loop {
        let size = baseline
            .read(&mut buffer)
            .map_err(|error| state_error("baseline checksum", error))?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    if hex::encode(digest.finalize()) != manifest.baseline_sha256 {
        return Err(state_error(
            "baseline checksum",
            "checkpoint bytes differ from the completed manifest",
        ));
    }
    baseline
        .rewind()
        .map_err(|error| state_error("baseline rewind", error))?;
    Ok(baseline)
}

fn visit_baseline(
    generation: &JournalDirectory,
    manifest: &Manifest,
    visit: &mut impl FnMut(RecoveryRecord) -> DbResult<()>,
) -> DbResult<()> {
    let baseline = checked_baseline(generation, manifest)?;
    let mut reader = io::BufReader::new(&baseline);
    let mut receipts = 0u64;
    let mut contacts = 0u64;
    let mut projects = 0u64;
    let mut agents = 0u64;
    loop {
        let mut bytes = Vec::new();
        let size = reader
            .by_ref()
            .take(MAX_RECORD_BYTES + 1)
            .read_until(b'\n', &mut bytes)
            .map_err(|error| state_error("baseline read", error))?;
        if size == 0 {
            break;
        }
        if size as u64 > MAX_RECORD_BYTES || bytes.last() != Some(&b'\n') {
            return Err(state_error(
                "baseline read",
                "oversized or incomplete checkpoint record",
            ));
        }
        let document: RecordDocument = serde_json::from_slice(&bytes)
            .map_err(|error| state_error("baseline decode", error))?;
        validate_document(&document, &manifest.generation)?;
        match &document.record {
            RecoveryRecord::Project(_) => projects += 1,
            RecoveryRecord::Agent(_) => agents += 1,
            RecoveryRecord::Receipt(_) => receipts += 1,
            RecoveryRecord::Contact(_) => contacts += 1,
        }
        visit(document.record)?;
    }
    if projects != manifest.projects
        || agents != manifest.agents
        || receipts != manifest.receipts
        || contacts != manifest.contacts
    {
        return Err(state_error(
            "baseline count",
            "checkpoint record counts differ from the completed manifest",
        ));
    }
    generation
        .validate_file(BASELINE_FILE, &baseline)
        .map_err(|error| state_error("baseline binding", error))?;
    Ok(())
}

fn visit_incremental_records(
    directory: &JournalDirectory,
    generation: &str,
    visit: &mut impl FnMut(RecoveryRecord) -> DbResult<()>,
) -> DbResult<IncrementalInventory> {
    let mut inventory = IncrementalInventory::empty(generation);
    for entry in std::fs::read_dir(directory.directory_path())
        .map_err(|error| state_error("record inventory", error))?
    {
        let entry = entry.map_err(|error| state_error("record inventory", error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(state_error(
                "record inventory",
                "non-UTF-8 recovery filename",
            ));
        };
        if !name.starts_with("receipt-") && !name.starts_with("contact-") {
            continue;
        }
        let (document, digest) =
            read_incremental_record(directory, name, generation)?.ok_or_else(|| {
                state_error(
                    "record inventory",
                    "recovery record disappeared during inspection",
                )
            })?;
        inventory.replace(None, &digest)?;
        visit(document.record)?;
    }
    Ok(inventory)
}

/// Visit the checked baseline first, followed by per-key updates from its one
/// active generation.
///
/// Missing legacy metadata is `None`; malformed or incomplete
/// metadata is an error and must never be promoted as a successful recovery.
pub fn visit_records(
    root: &Path,
    visit: impl FnMut(RecoveryRecord) -> DbResult<()>,
) -> DbResult<Option<Manifest>> {
    visit_records_with_inventory(root, visit).map(|records| records.map(|(manifest, _)| manifest))
}

pub(crate) fn visit_records_with_inventory(
    root: &Path,
    mut visit: impl FnMut(RecoveryRecord) -> DbResult<()>,
) -> DbResult<Option<(Manifest, IncrementalInventory)>> {
    let publication_before = read_revision_witness(root).ok().flatten();
    let directory = match JournalDirectory::open(root, DIRECTORY, false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(state_error("directory", error)),
    };
    let Some(manifest) = read_manifest(&directory)? else {
        for entry in std::fs::read_dir(directory.directory_path())
            .map_err(|error| state_error("incomplete checkpoint inventory", error))?
        {
            let entry =
                entry.map_err(|error| state_error("incomplete checkpoint inventory", error))?;
            if entry.file_name().to_string_lossy().starts_with('g') {
                return Err(state_error(
                    "manifest",
                    "generation files exist without a completed active checkpoint",
                ));
            }
        }
        return Ok(None);
    };
    let generation = JournalDirectory::open(
        &directory.directory_path(),
        &generation_directory(&manifest.generation)?,
        false,
    )
    .map_err(|error| state_error("generation directory", error))?;
    visit_baseline(&generation, &manifest, &mut visit)?;
    let inventory = visit_incremental_records(&generation, &manifest.generation, &mut visit)?;
    if read_manifest(&directory)?.as_ref() != Some(&manifest) {
        return Err(state_error(
            "manifest",
            "active generation changed while reading recovery state",
        ));
    }
    let publication_after = read_revision_witness(root).ok().flatten();
    if let Some(before) = publication_before
        && publication_after.as_ref() == Some(&before)
        && before.matches_manifest(&manifest)
        && !before.matches_inventory(&inventory)
    {
        return Err(state_error(
            "incremental inventory",
            "record count or contents differ from the sealed publication",
        ));
    }
    Ok(Some((manifest, inventory)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> crate::CanonicalDbConn {
        let conn = crate::CanonicalDbConn::open_memory().unwrap();
        conn.execute_raw(
            "CREATE TABLE db_identity (singleton INTEGER PRIMARY KEY, generation_id TEXT NOT NULL);
             CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL);
             CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, created_ts INTEGER NOT NULL, subject TEXT NOT NULL);
             CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id));
             CREATE TABLE project_mailbox_receipts (message_id INTEGER, agent_id INTEGER, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id));
             CREATE TABLE agent_links (id INTEGER PRIMARY KEY, a_project_id INTEGER, a_agent_id INTEGER, b_project_id INTEGER, b_agent_id INTEGER, status TEXT, reason TEXT, created_ts INTEGER, updated_ts INTEGER, expires_ts INTEGER);
             INSERT INTO projects VALUES (7, 'sender-project', '/work/sender'), (19, 'recipient-project', '/work/recipient');
             INSERT INTO agents VALUES (17, 7, 'BlueLake'), (29, 19, 'RedFox');
             INSERT INTO messages VALUES (41, 19, 100, 'Cross-project message'), (52, 19, 200, 'Shared project message');
             INSERT INTO message_recipients VALUES (41, 29, 111, NULL);
             INSERT INTO project_mailbox_receipts VALUES (52, 29, 222, 333);
             INSERT INTO agent_links VALUES (93, 7, 17, 19, 29, 'ApPrOvEd', 'review', 10, 20, 999999);",
        ).unwrap();
        conn
    }

    fn records(root: &Path) -> (Manifest, Vec<RecoveryRecord>) {
        let mut records = Vec::new();
        let manifest = visit_records(root, |record| {
            records.push(record);
            Ok(())
        })
        .unwrap()
        .unwrap();
        (manifest, records)
    }

    fn assert_sealed_inventory(root: &Path) -> IncrementalInventory {
        let witness = read_revision_witness(root).unwrap().unwrap();
        let (manifest, inventory) = visit_records_with_inventory(root, |_| Ok(()))
            .unwrap()
            .unwrap();
        assert!(witness.matches_manifest(&manifest));
        assert!(witness.matches_inventory(&inventory));
        inventory
    }

    fn direct_receipt_filename(conn: &crate::CanonicalDbConn) -> String {
        let rows = query(
            conn,
            &format!(
                "{} WHERE r.message_id = 41 AND r.agent_id = 29",
                receipt_select(false)
            ),
            &[],
        )
        .unwrap();
        record_name(&RecoveryRecord::Receipt(receipt(&rows[0], false).unwrap())).unwrap()
    }

    #[test]
    fn incremental_seal_resumes_interrupted_record_creation_and_replacement() {
        for existing in [false, true] {
            for after_replace in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let conn = database();
                let manifest = ensure_checkpoint(&conn, root.path()).unwrap();
                if existing {
                    persist_receipts(&conn, root.path(), 29, &[41]).unwrap();
                }
                let generation = root
                    .path()
                    .join(DIRECTORY)
                    .join(generation_directory(&manifest.generation).unwrap());
                let path = generation.join(direct_receipt_filename(&conn));
                let before = std::fs::read(&path).ok();
                conn.execute_raw(
                    "UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41; \
                     UPDATE agent_links SET status = 'blocked', updated_ts = 7 WHERE id = 93;",
                )
                .unwrap();
                if after_replace {
                    AFTER_RECORD_REPLACE_TEST_HOOK.with(|hook| {
                        *hook.borrow_mut() =
                            Some(Box::new(|| panic!("stop after record replacement")));
                    });
                } else {
                    AFTER_RECORD_INTENT_TEST_HOOK.with(|hook| {
                        *hook.borrow_mut() = Some(Box::new(|| panic!("stop after record intent")));
                    });
                }
                let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    persist_receipts(&conn, root.path(), 29, &[41])
                }));
                assert!(interrupted.is_err());
                assert!(read_revision_witness(root.path()).unwrap().is_none());
                let actual = std::fs::read(&path).ok();
                if after_replace {
                    assert_ne!(actual, before, "the real record replacement completed");
                    let document: RecordDocument = serde_json::from_slice(
                        actual.as_ref().expect("replacement published the record"),
                    )
                    .unwrap();
                    assert!(matches!(document.record, RecoveryRecord::Receipt(receipt)
                        if receipt.ack_ts == Some(444)));
                } else {
                    assert_eq!(actual, before, "only the durable intent was published");
                }

                // A different operation must finish all committed receipt debt
                // after validating the exact interrupted before/after state.
                persist_contact(&conn, root.path(), 17, 29).unwrap();
                assert_sealed_inventory(root.path());
                let recovered = records(root.path()).1;
                assert!(recovered.iter().any(|record| matches!(record,
                    RecoveryRecord::Receipt(receipt)
                        if receipt.message_id == 41 && receipt.ack_ts == Some(444)
                )));
                assert!(recovered.iter().any(|record| matches!(record,
                    RecoveryRecord::Contact(contact) if contact.status == "blocked"
                )));
            }
        }
    }

    #[test]
    fn interrupted_seal_refuses_changed_current_or_missing_untouched_records() {
        for missing_untouched in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let conn = database();
            let manifest = ensure_checkpoint(&conn, root.path()).unwrap();
            persist_receipts(&conn, root.path(), 29, &[41]).unwrap();
            persist_contact(&conn, root.path(), 17, 29).unwrap();
            conn.execute_raw("UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41")
                .unwrap();
            AFTER_RECORD_REPLACE_TEST_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(|| panic!("stop before completing seal")));
            });
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    persist_receipts(&conn, root.path(), 29, &[41])
                }))
                .is_err()
            );
            let directory = JournalDirectory::open(root.path(), DIRECTORY, false).unwrap();
            let pending =
                std::fs::read(root.path().join(DIRECTORY).join(READ_REVISION_FILE)).unwrap();
            let generation = JournalDirectory::open(
                &directory.directory_path(),
                &generation_directory(&manifest.generation).unwrap(),
                false,
            )
            .unwrap();
            let receipt_name = direct_receipt_filename(&conn);
            let (mut document, _) =
                read_incremental_record(&generation, &receipt_name, &manifest.generation)
                    .unwrap()
                    .unwrap();
            let (damaged_path, preserved) = if missing_untouched {
                let contact_name = std::fs::read_dir(generation.directory_path())
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .find(|name| name.starts_with("contact-"))
                    .unwrap();
                let path = generation.directory_path().join(contact_name);
                let preserved = std::fs::read(&path).unwrap();
                std::fs::rename(&path, root.path().join("held-contact.json")).unwrap();
                (path, preserved)
            } else {
                let path = generation.directory_path().join(&receipt_name);
                let preserved = std::fs::read(&path).unwrap();
                let RecoveryRecord::Receipt(receipt) = &mut document.record else {
                    panic!("fixture must contain a direct receipt");
                };
                receipt.ack_ts = Some(999);
                std::fs::write(
                    &path,
                    encode_record(&manifest.generation, document.record).unwrap(),
                )
                .unwrap();
                (path, preserved)
            };
            let failure = persist_contact(&conn, root.path(), 17, 29)
                .expect_err("reconciliation must preserve the earlier expected set");
            let expected = if missing_untouched {
                "record count or contents differ"
            } else {
                "neither its before nor after"
            };
            assert!(failure.to_string().contains(expected), "{failure}");
            assert_eq!(
                std::fs::read(root.path().join(DIRECTORY).join(READ_REVISION_FILE)).unwrap(),
                pending,
                "a failed resume must not replace its original recovery evidence",
            );
            assert!(read_revision_witness(root.path()).unwrap().is_none());
            std::fs::write(&damaged_path, preserved).unwrap();
            persist_contact(&conn, root.path(), 17, 29).unwrap();
            assert_sealed_inventory(root.path());
        }
    }

    #[test]
    fn incremental_seal_preserves_old_generation_until_activation_and_resumes_new_baseline() {
        let root = tempfile::tempdir().unwrap();
        let old = database();
        let initial = ensure_checkpoint(&old, root.path()).unwrap();
        old.execute_raw("UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41")
            .unwrap();
        persist_receipts(&old, root.path(), 29, &[41]).unwrap();
        let original = assert_sealed_inventory(root.path());
        assert_eq!(original.count, 1);

        let broken = database();
        broken.execute_raw("DROP TABLE agents").unwrap();
        ensure_checkpoint(&broken, root.path()).expect_err("the new baseline cannot complete");
        let directory = JournalDirectory::open(root.path(), DIRECTORY, false).unwrap();
        let interrupted = read_revision(&directory).unwrap().unwrap();
        assert!(interrupted.manifest.is_none());
        assert_eq!(interrupted.inventory.as_ref(), Some(&original));
        assert_eq!(read_manifest(&directory).unwrap().unwrap(), initial);
        persist_receipts(&old, root.path(), 29, &[41]).unwrap();
        assert_sealed_inventory(root.path());

        let replacement = database();
        AFTER_CHECKPOINT_ACTIVATION_TEST_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|| panic!("stop after generation activation")));
        });
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ensure_checkpoint(&replacement, root.path())
            }))
            .is_err()
        );
        let active = read_manifest(&directory).unwrap().unwrap();
        assert_ne!(active.generation, initial.generation);
        let interrupted = read_revision(&directory).unwrap().unwrap();
        assert!(interrupted.manifest.is_none());
        assert_eq!(interrupted.generation, active.generation);
        assert_eq!(
            interrupted.inventory.as_ref().unwrap().generation,
            initial.generation,
            "the pending marker still carries the prior generation's expected set",
        );
        persist_contact(&replacement, root.path(), 17, 29).unwrap();
        let restored = assert_sealed_inventory(root.path());
        assert_eq!(restored.generation, active.generation);
        assert_eq!(
            read_revision_witness(root.path())
                .unwrap()
                .unwrap()
                .generation(),
            active.generation
        );
        assert!(
            ensure_checkpoint(&old, root.path()).is_err(),
            "superseded authority stays retired"
        );
    }

    #[test]
    fn legacy_revision_is_unsealed_until_live_reconciliation_migrates_its_inventory() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        conn.execute_raw("UPDATE project_mailbox_receipts SET read_ts = NULL, ack_ts = NULL")
            .unwrap();
        let manifest = ensure_checkpoint(&conn, root.path()).unwrap();
        persist_receipts(&conn, root.path(), 29, &[41]).unwrap();
        let directory = JournalDirectory::open(root.path(), DIRECTORY, false).unwrap();
        let mut legacy = read_revision(&directory).unwrap().unwrap();
        legacy.schema_version = 1;
        legacy.inventory = None;
        legacy.pending_record = None;
        write_revision(&LockedState::open(root.path(), false).unwrap(), &legacy).unwrap();
        assert!(read_revision_witness(root.path()).unwrap().is_none());
        assert_eq!(
            records(root.path()).0,
            manifest,
            "legacy history remains inspectable"
        );

        conn.execute_raw(
            "UPDATE project_mailbox_receipts SET read_ts = 222, ack_ts = 555; \
             UPDATE agent_links SET status = 'blocked', updated_ts = 7 WHERE id = 93;",
        )
        .unwrap();
        persist_receipts(&conn, root.path(), 29, &[41]).unwrap();
        let complete = read_revision(&directory).unwrap().unwrap();
        assert_eq!(complete.schema_version, READ_REVISION_VERSION);
        assert!(complete.pending_record.is_none());
        assert_sealed_inventory(root.path());
        let recovered = records(root.path()).1;
        assert!(recovered.iter().any(|record| matches!(record,
            RecoveryRecord::Receipt(receipt) if receipt.shared && receipt.ack_ts == Some(555)
        )));
        assert!(recovered.iter().any(|record| matches!(record,
            RecoveryRecord::Contact(contact) if contact.status == "blocked"
        )));
    }

    #[test]
    fn healthy_selected_write_keeps_missing_unrelated_record_in_the_expected_seal() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        let manifest = ensure_checkpoint(&conn, root.path()).unwrap();
        persist_receipts(&conn, root.path(), 29, &[41]).unwrap();
        persist_contact(&conn, root.path(), 17, 29).unwrap();
        let before = assert_sealed_inventory(root.path());
        let generation = root
            .path()
            .join(DIRECTORY)
            .join(generation_directory(&manifest.generation).unwrap());
        let contact = std::fs::read_dir(&generation)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("contact-")
            })
            .unwrap();
        std::fs::rename(&contact, root.path().join("held-contact.json")).unwrap();
        conn.execute_raw("UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41")
            .unwrap();
        persist_receipts(&conn, root.path(), 29, &[41]).unwrap();
        let witness = read_revision_witness(root.path()).unwrap().unwrap();
        assert_eq!(
            witness.inventory.count, before.count,
            "untouched keys remain expected"
        );
        let failure = visit_records(root.path(), |_| Ok(()))
            .expect_err("a selected-key update cannot reseal a damaged subset");
        assert!(
            failure
                .to_string()
                .contains("record count or contents differ"),
            "{failure}"
        );
        assert!(
            ensure_checkpoint(&conn, root.path()).is_err(),
            "startup also refuses the subset"
        );
    }

    #[test]
    fn read_revision_tracks_checkpoint_and_each_writer_without_git_publication() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_revision_witness(root.path()).unwrap(), None);
        assert!(!root.path().join(DIRECTORY).exists());
        let conn = database();
        let baseline = ensure_checkpoint(&conn, root.path()).unwrap();
        let mut previous = read_revision_witness(root.path()).unwrap().unwrap();
        assert_eq!(previous.manifest, baseline);
        assert_eq!(
            read_revision_witness(root.path()).unwrap().as_ref(),
            Some(&previous),
            "read-only witness probes do not invalidate a snapshot"
        );
        assert_eq!(ensure_checkpoint(&conn, root.path()).unwrap(), baseline);
        let next = read_revision_witness(root.path()).unwrap().unwrap();
        assert_ne!(next, previous, "successful checkpoint has a fresh revision");
        previous = next;

        conn.execute_raw("UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41")
            .unwrap();
        persist_receipts(&conn, root.path(), 29, &[41]).unwrap();
        let next = read_revision_witness(root.path()).unwrap().unwrap();
        assert_ne!(
            next, previous,
            "direct receipt publication invalidates readers"
        );
        previous = next;
        conn.execute_raw("UPDATE project_mailbox_receipts SET ack_ts = 555 WHERE message_id = 52")
            .unwrap();
        persist_project_receipts(&conn, root.path(), 19, 29).unwrap();
        let next = read_revision_witness(root.path()).unwrap().unwrap();
        assert_ne!(
            next, previous,
            "bulk receipt publication invalidates readers"
        );
        previous = next;
        conn.execute_raw("UPDATE agent_links SET status = 'blocked', updated_ts = 7 WHERE id = 93")
            .unwrap();
        persist_contact(&conn, root.path(), 17, 29).unwrap();
        let next = read_revision_witness(root.path()).unwrap().unwrap();
        assert_ne!(next, previous, "contact publication invalidates readers");
        assert_eq!(next.manifest, baseline);
        assert!(!root.path().join(".git").exists());
        assert!(
            std::fs::metadata(root.path().join(DIRECTORY).join(READ_REVISION_FILE))
                .unwrap()
                .len()
                < MAX_READ_REVISION_BYTES
        );
    }

    #[test]
    fn read_revision_stays_uncacheable_after_partial_publication_until_retry() {
        struct FailShared<'a>(&'a crate::CanonicalDbConn);
        impl SyncQuery for FailShared<'_> {
            fn query_sync(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<Row>, sqlmodel_core::Error> {
                if sql.contains("FROM project_mailbox_receipts r") {
                    return Err(sqlmodel_core::Error::Custom(
                        "injected shared receipt read failure".to_string(),
                    ));
                }
                self.0.query_sync(sql, params)
            }

            fn execute_raw(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.0.execute_raw(sql)
            }
        }

        let root = tempfile::tempdir().unwrap();
        let conn = database();
        ensure_checkpoint(&conn, root.path()).unwrap();
        let before = read_revision_witness(root.path()).unwrap().unwrap();
        conn.execute_raw("UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41")
            .unwrap();
        let failure = persist_receipts(&FailShared(&conn), root.path(), 29, &[41]).unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("injected shared receipt read failure")
        );
        assert_eq!(read_revision_witness(root.path()).unwrap(), None);
        let directory = JournalDirectory::open(root.path(), DIRECTORY, false).unwrap();
        let pending = read_revision(&directory).unwrap().unwrap();
        assert!(pending.manifest.is_none());
        assert_ne!(pending.revision, before.revision);
        assert!(records(root.path()).1.iter().any(|record| matches!(record,
            RecoveryRecord::Receipt(receipt) if receipt.message_id == 41 && receipt.ack_ts == Some(444)
        )), "the failure follows an actual durable per-key record change");
        with_archive_publication(root.path(), &[], |paths| {
            assert!(paths.contains(&format!("{DIRECTORY}/{READ_REVISION_FILE}")));
            Ok(())
        })
        .unwrap();

        persist_receipts(&conn, root.path(), 29, &[41]).unwrap();
        let after = read_revision_witness(root.path()).unwrap().unwrap();
        assert_ne!(after.revision, pending.revision);
        assert_ne!(after, before);
    }

    #[test]
    fn interrupted_receipt_publication_is_reconciled_before_unrelated_success() {
        for retry in ["contact", "startup", "receipt"] {
            let root = tempfile::tempdir().unwrap();
            let conn = database();
            let baseline = ensure_checkpoint(&conn, root.path()).unwrap();
            conn.execute_raw(
                "UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41; \
                 UPDATE project_mailbox_receipts SET ack_ts = 555 WHERE message_id = 52; \
                 UPDATE agent_links SET status = 'blocked', updated_ts = 15; \
                 INSERT INTO projects VALUES (31, 'new-project', '/work/new'); \
                 INSERT INTO agents VALUES (37, 31, 'GreenLake'); \
                 CREATE TEMP VIEW project_mailbox_receipts AS \
                 SELECT * FROM missing_shared_receipts;",
            )
            .unwrap();
            let failure = persist_receipts(&conn, root.path(), 29, &[41, 52])
                .expect_err("the real shared query fails after direct publication");
            assert!(
                failure.to_string().contains("receipt snapshot"),
                "{failure}"
            );
            assert_eq!(read_revision_witness(root.path()).unwrap(), None);
            assert!(records(root.path()).1.iter().any(|record| matches!(
                record,
                RecoveryRecord::Receipt(receipt)
                    if receipt.message_id == 41 && receipt.ack_ts == Some(444)
            )));
            let shared = conn
                .query_sync(
                    "SELECT ack_ts FROM main.project_mailbox_receipts WHERE message_id = 52",
                    &[],
                )
                .unwrap();
            assert_eq!(shared[0].get_named::<i64>("ack_ts").unwrap(), 555);

            let failure = persist_contact(&conn, root.path(), 17, 29)
                .expect_err("an unrelated write must not seal incomplete receipts");
            assert!(
                failure
                    .to_string()
                    .contains("incomplete publication reconciliation"),
                "{failure}"
            );
            assert_eq!(read_revision_witness(root.path()).unwrap(), None);
            conn.execute_raw("DROP VIEW temp.project_mailbox_receipts")
                .unwrap();

            match retry {
                "contact" => persist_contact(&conn, root.path(), 17, 29),
                "startup" => ensure_checkpoint(&conn, root.path()).map(|_| ()),
                // Retrying only the direct message must also settle the
                // earlier failure for the different shared message.
                _ => persist_receipts(&conn, root.path(), 29, &[41]),
            }
            .unwrap();
            let witness = read_revision_witness(root.path()).unwrap().unwrap();
            assert!(witness.matches_manifest(&baseline));
            let (manifest, recovered) = records(root.path());
            assert_eq!(manifest, baseline, "repair retains the original baseline");
            for (id, shared, expected_ack) in [(41, false, 444), (52, true, 555)] {
                let latest = recovered
                    .iter()
                    .rev()
                    .find_map(|record| match record {
                        RecoveryRecord::Receipt(receipt)
                            if receipt.message_id == id && receipt.shared == shared =>
                        {
                            Some(receipt)
                        }
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(latest.ack_ts, Some(expected_ack), "{retry}");
            }
            let latest = recovered
                .iter()
                .rev()
                .find_map(|record| match record {
                    RecoveryRecord::Contact(contact) if contact.id == 93 => Some(contact),
                    _ => None,
                })
                .unwrap();
            assert_eq!(latest.status, "blocked", "{retry}");
            assert_eq!(
                latest.updated_ts, 15,
                "clock order must not restore approval"
            );
        }
    }

    #[test]
    fn incomplete_publication_cannot_hide_receipts_with_missing_identity_joins() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        ensure_checkpoint(&conn, root.path()).unwrap();
        conn.execute_raw(
            "UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41; \
             CREATE TEMP VIEW project_mailbox_receipts AS \
             SELECT * FROM missing_shared_receipts;",
        )
        .unwrap();
        persist_receipts(&conn, root.path(), 29, &[41]).unwrap_err();
        conn.execute_raw(
            "DROP VIEW temp.project_mailbox_receipts; \
             INSERT INTO project_mailbox_receipts VALUES (63, 29, 600, 700);",
        )
        .unwrap();

        let failure = persist_contact(&conn, root.path(), 17, 29)
            .expect_err("an omitted identity join must retain incomplete authority");
        assert!(
            failure
                .to_string()
                .contains("refusing incomplete identity joins"),
            "{failure}"
        );
        assert_eq!(read_revision_witness(root.path()).unwrap(), None);
        conn.execute_raw("INSERT INTO messages VALUES (63, 19, 300, 'Restored message identity')")
            .unwrap();
        persist_contact(&conn, root.path(), 17, 29).unwrap();
        assert!(read_revision_witness(root.path()).unwrap().is_some());
        assert!(records(root.path()).1.iter().any(|record| matches!(
            record,
            RecoveryRecord::Receipt(receipt)
                if receipt.message_id == 63 && receipt.ack_ts == Some(700)
        )));
    }

    #[test]
    fn complete_publications_keep_selected_receipt_and_contact_writes_bounded() {
        struct RefuseFullScan<'a>(&'a crate::CanonicalDbConn);
        impl SyncQuery for RefuseFullScan<'_> {
            fn query_sync(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<Row>, sqlmodel_core::Error> {
                if sql.contains("ORDER BY r.agent_id, r.message_id")
                    || sql.contains("WHERE l.id > ?")
                    || sql.contains("AS receipts")
                {
                    return Err(sqlmodel_core::Error::Custom(
                        "healthy selected writes must not scan all recovery state".to_string(),
                    ));
                }
                self.0.query_sync(sql, params)
            }

            fn execute_raw(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.0.execute_raw(sql)
            }
        }

        let root = tempfile::tempdir().unwrap();
        let conn = database();
        ensure_checkpoint(&conn, root.path()).unwrap();
        let guarded = RefuseFullScan(&conn);
        persist_receipts(&guarded, root.path(), 29, &[41]).unwrap();
        persist_contact(&guarded, root.path(), 17, 29).unwrap();
        assert!(read_revision_witness(root.path()).unwrap().is_some());
    }

    #[test]
    fn read_revision_refuses_malformed_oversized_and_mismatched_authority() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        ensure_checkpoint(&conn, root.path()).unwrap();
        let directory = JournalDirectory::open(root.path(), DIRECTORY, false).unwrap();
        let current = read_revision(&directory).unwrap().unwrap();
        for bytes in [
            b"malformed revision".to_vec(),
            vec![b' '; usize::try_from(MAX_READ_REVISION_BYTES + 1).unwrap()],
            serde_json::to_vec(&ReadRevision {
                generation: "different-generation".to_string(),
                ..current.clone()
            })
            .unwrap(),
        ] {
            directory
                .atomic_write(READ_REVISION_FILE, |file| file.write_all(&bytes))
                .unwrap();
            assert!(read_revision_witness(root.path()).is_err());
            assert_eq!(
                std::fs::read(root.path().join(DIRECTORY).join(READ_REVISION_FILE)).unwrap(),
                bytes,
                "a read probe must preserve malformed revision evidence"
            );
        }
        directory
            .atomic_write(READ_REVISION_FILE, |file| {
                file.write_all(&serde_json::to_vec(&current).unwrap())
            })
            .unwrap();
        let mut mismatched = current.manifest.clone().unwrap();
        mismatched.contacts += 1;
        directory
            .atomic_write(MANIFEST_FILE, |file| {
                file.write_all(&serde_json::to_vec(&mismatched).unwrap())
            })
            .unwrap();
        let failure = read_revision_witness(root.path()).unwrap_err();
        assert!(failure.to_string().contains("active checkpoint differs"));
    }

    #[test]
    fn older_checkpoints_remain_inspectable_without_a_cache_revision() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        let manifest = ensure_checkpoint(&conn, root.path()).unwrap();
        std::fs::rename(
            root.path().join(DIRECTORY).join(READ_REVISION_FILE),
            root.path().join("held-read-revision.json"),
        )
        .unwrap();
        assert_eq!(read_revision_witness(root.path()).unwrap(), None);
        assert_eq!(records(root.path()).0, manifest);
        assert!(
            !root
                .path()
                .join(DIRECTORY)
                .join(READ_REVISION_FILE)
                .exists()
        );
        ensure_checkpoint(&conn, root.path()).unwrap();
        assert!(read_revision_witness(root.path()).unwrap().is_some());
    }

    #[test]
    fn complete_baseline_and_incremental_state_preserve_receipts_and_contact_decisions() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        let baseline = ensure_checkpoint(&conn, root.path()).unwrap();
        assert_eq!(
            (
                baseline.projects,
                baseline.agents,
                baseline.receipts,
                baseline.contacts
            ),
            (2, 2, 2, 1)
        );
        let (_, initial) = records(root.path());
        assert!(matches!(&initial[0], RecoveryRecord::Project(project) if project.id == 7));
        assert!(
            matches!(&initial[2], RecoveryRecord::Agent(agent) if agent.id == 17 && agent.project.id == 7)
        );
        assert!(initial.iter().any(
            |record| matches!(record, RecoveryRecord::Contact(link) if link.status == "approved")
        ));

        // An older call may have captured the approval before a block committed.
        // Publication must reread the current decision under its own lock,
        // including when the caller's timestamp would have sorted newer.
        conn.execute_raw("UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41;
                          UPDATE agent_links SET status = 'blocked', updated_ts = 15, expires_ts = NULL WHERE id = 93;").unwrap();
        persist_receipts(&conn, root.path(), 29, &[41, 52, 999]).unwrap();
        persist_contact(&conn, root.path(), 17, 29).unwrap();
        let (after, recovered) = records(root.path());
        assert_eq!(
            after, baseline,
            "incremental writes do not rewrite the baseline"
        );
        let direct = recovered
            .iter()
            .rev()
            .find_map(|record| match record {
                RecoveryRecord::Receipt(receipt) if !receipt.shared => Some(receipt),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            (direct.message_id, direct.read_ts, direct.ack_ts),
            (41, Some(111), Some(444))
        );
        assert_eq!((direct.agent.id, direct.agent.project.id), (29, 19));
        assert_eq!(direct.message_project.human_key, "/work/recipient");
        let shared = recovered
            .iter()
            .rev()
            .find_map(|record| match record {
                RecoveryRecord::Receipt(receipt) if receipt.shared => Some(receipt),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            (shared.message_id, shared.read_ts, shared.ack_ts),
            (52, Some(222), Some(333))
        );
        let link = recovered
            .iter()
            .rev()
            .find_map(|record| match record {
                RecoveryRecord::Contact(link) => Some(link),
                _ => None,
            })
            .unwrap();
        assert_eq!(link.status, "blocked");
        assert_eq!(link.updated_ts, 15);
        assert_eq!(link.expires_ts, None);
        assert_eq!((link.from.project.id, link.to.project.id), (7, 19));
        assert!(!recovered.iter().any(
            |record| matches!(record, RecoveryRecord::Receipt(receipt) if receipt.message_id == 999)
        ));
    }

    #[test]
    fn checkpoint_streams_more_than_one_page_from_a_real_database() {
        struct Observed<'a> {
            conn: &'a crate::CanonicalDbConn,
            maximum_page: std::cell::Cell<usize>,
        }
        impl SyncQuery for Observed<'_> {
            fn query_sync(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<Row>, sqlmodel_core::Error> {
                let rows = self.conn.query_sync(sql, params)?;
                self.maximum_page
                    .set(self.maximum_page.get().max(rows.len()));
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.conn.execute_raw(sql)
            }
        }
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        conn.execute_raw(
            "WITH RECURSIVE ids(id) AS (SELECT 1000 UNION ALL SELECT id + 1 FROM ids WHERE id < 1599)
             INSERT INTO messages SELECT id, 19, id, 'Historical message' FROM ids;
             INSERT INTO message_recipients SELECT id, 29, 888, 999 FROM messages WHERE id >= 1000;",
        ).unwrap();
        let observed = Observed {
            conn: &conn,
            maximum_page: std::cell::Cell::new(0),
        };
        let manifest = checkpoint_all(&observed, root.path()).unwrap();
        assert_eq!(manifest.receipts, 602);
        assert_eq!(observed.maximum_page.get(), BATCH_ROWS);
        let (_, records) = records(root.path());
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, RecoveryRecord::Receipt(_)))
                .count(),
            602
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn production_pool_mutations_archive_committed_state_and_retry_failed_ack_publication() {
        use crate::queries;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("archive");
        let config = crate::DbPoolConfig {
            database_url: format!(
                "sqlite:///{}",
                temporary.path().join("mail.sqlite3").display()
            ),
            storage_root: Some(root.clone()),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&config).unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let (message_id, bulk_message_id, recipient_id, read_ts, ack_ts) = runtime.block_on(async {
            let cx = asupersync::Cx::current().unwrap();
            let from_project = queries::ensure_project(&cx, &pool, "/work/recovery-sender")
                .await
                .into_result()
                .unwrap()
                .id
                .unwrap();
            let to_project = queries::ensure_project(&cx, &pool, "/work/recovery-recipient")
                .await
                .into_result()
                .unwrap()
                .id
                .unwrap();
            let sender = queries::register_agent(
                &cx,
                &pool,
                from_project,
                "BlueLake",
                "test",
                "test",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .unwrap()
            .id
            .unwrap();
            let recipient = queries::register_agent(
                &cx, &pool, to_project, "RedFox", "test", "test", None, None, None,
            )
            .await
            .into_result()
            .unwrap()
            .id
            .unwrap();
            queries::request_contact(
                &cx,
                &pool,
                from_project,
                sender,
                to_project,
                recipient,
                "recovery probe",
                3600,
            )
            .await
            .into_result()
            .unwrap();
            queries::respond_contact(
                &cx,
                &pool,
                from_project,
                sender,
                to_project,
                recipient,
                true,
                3600,
            )
            .await
            .into_result()
            .unwrap();
            let message_id = queries::create_message_with_recipients(
                &cx,
                &pool,
                to_project,
                sender,
                "Recovery proof",
                "Body",
                None,
                "normal",
                true,
                "[]",
                &[(recipient, "to")],
            )
            .await
            .into_result()
            .unwrap()
            .id
            .unwrap();
            let read_ts = queries::mark_message_read(&cx, &pool, recipient, message_id)
                .await
                .into_result()
                .unwrap();

            let active_path = root.join(DIRECTORY).join(MANIFEST_FILE);
            let prior_manifest = std::fs::read(&active_path).unwrap();
            std::fs::write(&active_path, b"invalid recovery manifest").unwrap();
            let claim = crate::IdempotencyClaim {
                project_id: to_project,
                tool: "acknowledge_message",
                key: "recovery-ack",
                fingerprint: "recovery-ack-fingerprint",
            };
            let failed =
                queries::acknowledge_message_idempotent(&cx, &pool, recipient, message_id, claim)
                    .await
                    .into_result();
            assert!(
                failed.is_err(),
                "a database commit alone must not report recovery durability"
            );
            let journal = JournalDirectory::open(&root, DIRECTORY, false).unwrap();
            journal
                .atomic_write(MANIFEST_FILE, |file| file.write_all(&prior_manifest))
                .unwrap();
            let replay =
                queries::acknowledge_message_idempotent(&cx, &pool, recipient, message_id, claim)
                    .await
                    .into_result()
                    .unwrap();
            let crate::IdempotentOutcome::Replayed((replayed_read, ack_ts)) = replay else {
                panic!(
                    "failed archive publication must replay the committed ACK without mutating it"
                );
            };
            assert_eq!(replayed_read, read_ts);
            let bulk_message_id = queries::create_message_with_recipients(
                &cx,
                &pool,
                to_project,
                sender,
                "Bulk recovery proof",
                "Body",
                None,
                "normal",
                false,
                "[]",
                &[(recipient, "to")],
            )
            .await
            .into_result()
            .unwrap()
            .id
            .unwrap();
            std::fs::write(&active_path, b"invalid recovery manifest").unwrap();
            let failed_bulk = queries::mark_messages_read_bulk(
                &cx, &pool, to_project, recipient, None, 100,
            )
            .await
            .into_result();
            assert!(failed_bulk.is_err());
            journal
                .atomic_write(MANIFEST_FILE, |file| file.write_all(&prior_manifest))
                .unwrap();
            let retried_bulk = queries::mark_messages_read_bulk(
                &cx, &pool, to_project, recipient, None, 100,
            )
            .await
            .into_result()
            .unwrap();
            assert_eq!(
                retried_bulk.marked, 0,
                "retry must archive rows already marked read by the failed call"
            );
            assert!(!retried_bulk.more);
            queries::respond_contact(
                &cx,
                &pool,
                from_project,
                sender,
                to_project,
                recipient,
                false,
                3600,
            )
            .await
            .into_result()
            .unwrap();
            (message_id, bulk_message_id, recipient, read_ts, ack_ts)
        });
        drop(pool);
        // Consume only filesystem recovery artifacts: no DB connection or
        // salvage path is supplied to the reader after the real runtime writes.
        let (_, recovered) = records(&root);
        assert!(recovered.iter().any(|record| matches!(record,
            RecoveryRecord::Receipt(receipt)
                if receipt.message_id == message_id && receipt.agent.id == recipient_id
                    && receipt.read_ts == Some(read_ts) && receipt.ack_ts == Some(ack_ts)
        )));
        assert!(recovered.iter().any(|record| matches!(record,
            RecoveryRecord::Receipt(receipt)
                if receipt.message_id == bulk_message_id && receipt.agent.id == recipient_id
                    && receipt.read_ts.is_some() && receipt.ack_ts.is_none()
        )));
        assert!(recovered.iter().any(|record| matches!(record,
            RecoveryRecord::Contact(link) if link.status == "blocked"
                && link.from.project.id != link.to.project.id
        )));
    }

    #[test]
    fn corrupted_or_incomplete_metadata_never_becomes_successful_recovery() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        let manifest = ensure_checkpoint(&conn, root.path()).unwrap();
        let baseline = root
            .path()
            .join(DIRECTORY)
            .join(generation_directory(&manifest.generation).unwrap())
            .join(BASELINE_FILE);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&baseline)
            .unwrap();
        file.write_all(b"truncated-record").unwrap();
        file.sync_all().unwrap();
        let mut visited = 0;
        let error = visit_records(root.path(), |_| {
            visited += 1;
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("checksum"));
        assert!(is_publication_error(&error));
        assert!(!is_publication_error(&DbError::Sqlite(
            "database is locked".to_string()
        )));
        assert_eq!(
            visited, 0,
            "validate completed baseline before yielding state"
        );

        let incomplete = tempfile::tempdir().unwrap();
        let corrupt_db = database();
        corrupt_db
            .execute_raw("DELETE FROM projects WHERE id = 7;")
            .unwrap();
        assert!(ensure_checkpoint(&corrupt_db, incomplete.path()).is_err());
        assert!(
            !incomplete
                .path()
                .join(DIRECTORY)
                .join(MANIFEST_FILE)
                .exists()
        );
        assert!(visit_records(incomplete.path(), |_| Ok(())).is_err());
        assert_eq!(read_revision_witness(incomplete.path()).unwrap(), None);
        corrupt_db
            .execute_raw("INSERT INTO projects VALUES (7, 'sender-project', '/work/sender')")
            .unwrap();
        ensure_checkpoint(&corrupt_db, incomplete.path()).unwrap();
        assert!(read_revision_witness(incomplete.path()).unwrap().is_some());
    }

    #[test]
    fn new_database_generation_does_not_inherit_prior_receipts_or_contacts() {
        let root = tempfile::tempdir().unwrap();
        let first = database();
        let old = ensure_checkpoint(&first, root.path()).unwrap();
        first
            .execute_raw("UPDATE message_recipients SET ack_ts = 444;")
            .unwrap();
        persist_receipts(&first, root.path(), 29, &[41]).unwrap();
        let replacement = database();
        replacement
            .execute_raw(
                "UPDATE message_recipients SET read_ts = NULL;
                                 DELETE FROM project_mailbox_receipts;
                                 DELETE FROM agent_links;",
            )
            .unwrap();
        let new = ensure_checkpoint(&replacement, root.path()).unwrap();
        let replacement_revision = read_revision_witness(root.path()).unwrap();
        assert_ne!(old.generation, new.generation);
        let (_, restored) = records(root.path());
        assert!(restored.iter().all(|record| matches!(
            record,
            RecoveryRecord::Project(_) | RecoveryRecord::Agent(_)
        )));
        assert!(
            root.path()
                .join(DIRECTORY)
                .join(generation_directory(&old.generation).unwrap())
                .exists()
        );
        assert!(
            ensure_checkpoint(&first, root.path()).is_err(),
            "a stale database cannot silently reactivate superseded state"
        );
        assert_eq!(
            read_revision_witness(root.path()).unwrap(),
            replacement_revision,
            "a superseded writer must not invalidate the active generation"
        );
    }

    #[test]
    fn superseded_baseline_only_generation_is_refused_but_interrupted_baseline_retries() {
        let root = tempfile::tempdir().unwrap();
        let first = database();
        let old = ensure_checkpoint(&first, root.path()).unwrap();
        let replacement = database();
        let new_generation = ensure_generation(&replacement).unwrap();
        let state = LockedState::open(root.path(), true).unwrap();
        let interrupted = state.generation(&new_generation, true).unwrap();
        interrupted
            .atomic_write(BASELINE_FILE, |file| file.write_all(b"unfinished baseline"))
            .unwrap();
        drop(state);
        let new = ensure_checkpoint(&replacement, root.path()).unwrap();
        assert_ne!(old.generation, new.generation);
        let failure = ensure_checkpoint(&first, root.path()).unwrap_err();
        assert!(failure.to_string().contains("superseded"));
        let (active, _) = records(root.path());
        assert_eq!(active, new);
    }

    #[test]
    fn git_publication_always_contains_the_current_generation_baseline() {
        let root = tempfile::tempdir().unwrap();
        let first = database();
        let old = ensure_checkpoint(&first, root.path()).unwrap();
        let replacement = database();
        let new = ensure_checkpoint(&replacement, root.path()).unwrap();
        let old_path = format!(
            "{DIRECTORY}/{}/{BASELINE_FILE}",
            generation_directory(&old.generation).unwrap()
        );
        let expected = format!(
            "{DIRECTORY}/{}/{BASELINE_FILE}",
            generation_directory(&new.generation).unwrap()
        );
        with_archive_publication(root.path(), &[old_path], |paths| {
            assert!(paths.contains(&expected));
            assert!(paths.contains(&format!("{DIRECTORY}/{MANIFEST_FILE}")));
            assert!(paths.contains(&format!("{DIRECTORY}/{READ_REVISION_FILE}")));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn git_publication_uses_durable_debt_and_retains_it_on_failure_or_unwind() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        ensure_checkpoint(&conn, root.path()).unwrap();
        with_archive_publication(root.path(), &[], |_| Ok(())).unwrap();
        let journal = root.path().join(DIRECTORY).join(GIT_PENDING_FILE);
        assert_eq!(std::fs::read(&journal).unwrap().as_slice(), b"");

        conn.execute_raw("UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41")
            .unwrap();
        persist_receipts(&conn, root.path(), 29, &[41]).unwrap();
        let debt = std::fs::read(&journal).unwrap();
        assert_ne!(debt.as_slice(), b"");
        let mut first_paths = Vec::new();
        let failure = with_archive_publication(root.path(), &[], |paths| {
            first_paths = paths.to_vec();
            Err::<(), _>(DbError::Internal("commit refused".to_string()))
        });
        assert!(failure.is_err());
        assert_eq!(std::fs::read(&journal).unwrap(), debt);
        assert_eq!(
            first_paths
                .iter()
                .filter(|path| path.contains("/receipt-"))
                .count(),
            1
        );

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_archive_publication(root.path(), &[], |_| -> DbResult<()> {
                panic!("publication interrupted after selecting paths")
            })
        }));
        assert!(panic.is_err());
        assert_eq!(std::fs::read(&journal).unwrap(), debt);

        // A second key was published after the failed attempt's hint snapshot.
        // An empty hint list must still publish it with the latest revision.
        conn.execute_raw(
            "UPDATE agent_links SET status = 'blocked', updated_ts = 40 WHERE id = 93",
        )
        .unwrap();
        persist_contact(&conn, root.path(), 17, 29).unwrap();
        with_archive_publication(root.path(), &[], |paths| {
            assert!(first_paths.iter().all(|path| paths.contains(path)));
            assert_eq!(
                paths
                    .iter()
                    .filter(|path| path.contains("/contact-"))
                    .count(),
                1
            );
            assert!(paths.contains(&format!("{DIRECTORY}/{READ_REVISION_FILE}")));
            assert!(!paths.iter().any(|path| path.ends_with(GIT_PENDING_FILE)));
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&journal).unwrap().as_slice(), b"");

        // Healthy publications do not rescan all prior per-key artifacts.
        conn.execute_raw("UPDATE project_mailbox_receipts SET read_ts = 777 WHERE message_id = 52")
            .unwrap();
        persist_receipts(&conn, root.path(), 29, &[52]).unwrap();
        with_archive_publication(root.path(), &[], |paths| {
            assert_eq!(
                paths
                    .iter()
                    .filter(|path| path.contains("/receipt-"))
                    .count(),
                1
            );
            assert!(!paths.iter().any(|path| path.contains("/contact-")));
            assert!(
                !first_paths
                    .iter()
                    .filter(|path| path.contains("/receipt-"))
                    .any(|path| paths.contains(path))
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn git_publication_distinguishes_abandoned_switch_intent_from_missing_authority() {
        for scenario in [
            "unactivated_baseline",
            "current_superseded",
            "current_receipt",
            "past_superseded",
        ] {
            let root = tempfile::tempdir().unwrap();
            let first = database();
            let original = ensure_checkpoint(&first, root.path()).unwrap();
            with_archive_publication(root.path(), &[], |_| Ok(())).unwrap();
            let generation = generation_directory(&original.generation).unwrap();
            let missing = match scenario {
                "unactivated_baseline" => {
                    let replacement = database();
                    // Fail the real replacement checkpoint after it has logged
                    // write-ahead paths, before it can publish its baseline.
                    replacement.execute_raw("DROP TABLE agents").unwrap();
                    assert!(ensure_checkpoint(&replacement, root.path()).is_err());
                    let replacement_generation = ensure_generation(&replacement).unwrap();
                    ensure_checkpoint(&first, root.path()).unwrap();
                    format!(
                        "{DIRECTORY}/{}/{BASELINE_FILE}",
                        generation_directory(&replacement_generation).unwrap()
                    )
                }
                "current_superseded" => {
                    let state = LockedState::open(root.path(), false).unwrap();
                    let path = format!("{DIRECTORY}/{generation}/{SUPERSEDED_FILE}");
                    append_git_pending_path(&state, &path).unwrap();
                    path
                }
                "current_receipt" => {
                    first
                        .execute_raw(
                            "UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41",
                        )
                        .unwrap();
                    persist_receipts(&first, root.path(), 29, &[41]).unwrap();
                    let (_, recovered) = records(root.path());
                    let updated = recovered
                        .iter()
                        .find(|record| {
                            matches!(
                                record,
                                RecoveryRecord::Receipt(receipt)
                                    if receipt.message_id == 41 && receipt.ack_ts == Some(444)
                            )
                        })
                        .unwrap();
                    let path =
                        format!("{DIRECTORY}/{generation}/{}", record_name(updated).unwrap());
                    std::fs::rename(
                        root.path().join(&path),
                        root.path().join("held-receipt.json"),
                    )
                    .unwrap();
                    path
                }
                _ => {
                    ensure_checkpoint(&database(), root.path()).unwrap();
                    let path = format!("{DIRECTORY}/{generation}/{SUPERSEDED_FILE}");
                    std::fs::rename(
                        root.path().join(&path),
                        root.path().join("held-superseded.json"),
                    )
                    .unwrap();
                    path
                }
            };
            assert!(!root.path().join(&missing).exists(), "{scenario}");
            let journal = root.path().join(DIRECTORY).join(GIT_PENDING_FILE);
            let debt = std::fs::read(&journal).unwrap();
            let mut published = None;
            let result = with_archive_publication(root.path(), &[], |paths| {
                published = Some(paths.to_vec());
                Ok(())
            });
            if matches!(scenario, "unactivated_baseline" | "current_superseded") {
                result.unwrap();
                assert!(!published.unwrap().contains(&missing), "{scenario}");
                assert!(read_revision_witness(root.path()).unwrap().is_some());
            } else {
                assert!(result.is_err(), "{scenario}");
                assert!(published.is_none(), "{scenario}");
                assert_eq!(std::fs::read(&journal).unwrap(), debt, "{scenario}");
            }
        }
    }

    #[test]
    fn git_publication_inventories_a_checkout_without_its_runtime_outbox() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        ensure_checkpoint(&conn, root.path()).unwrap();
        persist_receipts(&conn, root.path(), 29, &[41, 52]).unwrap();
        persist_contact(&conn, root.path(), 17, 29).unwrap();
        std::fs::rename(
            root.path().join(DIRECTORY).join(GIT_PENDING_FILE),
            root.path().join("preserved-git-pending.jsonl"),
        )
        .unwrap();
        with_archive_publication(root.path(), &[], |paths| {
            assert_eq!(
                paths
                    .iter()
                    .filter(|path| path.contains("/receipt-"))
                    .count(),
                2
            );
            assert_eq!(
                paths
                    .iter()
                    .filter(|path| path.contains("/contact-"))
                    .count(),
                1
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(
            std::fs::read(root.path().join(DIRECTORY).join(GIT_PENDING_FILE))
                .unwrap()
                .as_slice(),
            b""
        );
    }

    #[test]
    fn git_publication_refuses_malformed_oversized_and_runtime_path_debt() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        ensure_checkpoint(&conn, root.path()).unwrap();
        let directory = JournalDirectory::open(root.path(), DIRECTORY, false).unwrap();
        for bytes in [
            b"{\"schema_version\":1,\"path\":null}".to_vec(),
            b"not-json\n".to_vec(),
            b"{\"schema_version\":2,\"path\":null}\n".to_vec(),
            b"{\"schema_version\":1,\"path\":\"recovery-state/.state.lock\"}\n".to_vec(),
            b"{\"schema_version\":1,\"path\":\"recovery-state/gvalid/../../outside\"}\n".to_vec(),
            vec![b' '; usize::try_from(MAX_GIT_PENDING_RECORD_BYTES + 1).unwrap()],
        ] {
            directory
                .atomic_write(GIT_PENDING_FILE, |file| file.write_all(&bytes))
                .unwrap();
            let mut published = false;
            assert!(
                with_archive_publication(root.path(), &[], |_| {
                    published = true;
                    Ok(())
                })
                .is_err()
            );
            assert!(!published);
            assert_eq!(
                std::fs::read(directory.directory_path().join(GIT_PENDING_FILE)).unwrap(),
                bytes
            );
        }
        directory
            .atomic_write(GIT_PENDING_FILE, |file| {
                file.set_len(MAX_GIT_PENDING_BYTES + 1)
            })
            .unwrap();
        assert!(with_archive_publication(root.path(), &[], |_| Ok(())).is_err());
        assert_eq!(
            std::fs::metadata(directory.directory_path().join(GIT_PENDING_FILE))
                .unwrap()
                .len(),
            MAX_GIT_PENDING_BYTES + 1
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_publication_outbox_symlink_preserves_foreign_bytes_and_receipts() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        ensure_checkpoint(&conn, root.path()).unwrap();
        let journal = root.path().join(DIRECTORY).join(GIT_PENDING_FILE);
        std::fs::rename(&journal, root.path().join("preserved-git-pending.jsonl")).unwrap();
        let foreign = root.path().join("foreign-evidence");
        std::fs::write(&foreign, b"unchanged foreign state").unwrap();
        std::os::unix::fs::symlink(&foreign, &journal).unwrap();
        let before = records(root.path());
        conn.execute_raw("UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41")
            .unwrap();
        assert!(persist_receipts(&conn, root.path(), 29, &[41]).is_err());
        assert!(with_archive_publication(root.path(), &[], |_| Ok(())).is_err());
        assert_eq!(records(root.path()), before);
        assert_eq!(std::fs::read(&foreign).unwrap(), b"unchanged foreign state");
    }

    #[cfg(unix)]
    #[test]
    fn read_revision_symlink_refuses_reads_and_writes_without_touching_target() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        ensure_checkpoint(&conn, root.path()).unwrap();
        let revision = root.path().join(DIRECTORY).join(READ_REVISION_FILE);
        std::fs::rename(&revision, root.path().join("held-read-revision.json")).unwrap();
        let outside = root.path().join("outside-revision-evidence");
        std::fs::write(&outside, b"preserve unrelated evidence").unwrap();
        std::os::unix::fs::symlink(&outside, &revision).unwrap();
        assert!(read_revision_witness(root.path()).is_err());
        let before = records(root.path());
        conn.execute_raw("UPDATE message_recipients SET ack_ts = 444 WHERE message_id = 41")
            .unwrap();
        assert!(persist_receipts(&conn, root.path(), 29, &[41]).is_err());
        assert_eq!(records(root.path()), before);
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"preserve unrelated evidence"
        );
    }

    #[cfg(unix)]
    #[test]
    fn incremental_symlink_is_refused_without_reading_its_target() {
        let root = tempfile::tempdir().unwrap();
        let conn = database();
        let manifest = ensure_checkpoint(&conn, root.path()).unwrap();
        let directory = root
            .path()
            .join(DIRECTORY)
            .join(generation_directory(&manifest.generation).unwrap());
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"must not be read as recovery state").unwrap();
        std::os::unix::fs::symlink(&outside, directory.join("receipt-alias.json")).unwrap();
        assert!(visit_records(root.path(), |_| Ok(())).is_err());
        assert_eq!(
            std::fs::read(outside).unwrap(),
            b"must not be read as recovery state"
        );
    }
}
