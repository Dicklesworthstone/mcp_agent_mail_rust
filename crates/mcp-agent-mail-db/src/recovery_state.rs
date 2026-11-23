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
const SCHEMA_VERSION: u32 = 1;
const BATCH_ROWS: usize = 256;
const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const ERROR_PREFIX: &str = "archive recovery state ";

static PENDING_ARCHIVE_PATHS: LazyLock<Mutex<BTreeMap<PathBuf, BTreeSet<String>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Hand completed state artifacts to the normal Git commit coalescer. Durable
/// files remain recoverable even if the process exits before this hint drains;
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
/// database operation. The database receipt may already have committed: after
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
    let mut file = match directory.open_file(name, JournalFileMode::Read) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(state_error("read", error)),
    };
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| state_error("read", error))?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(state_error("read", "record exceeds bounded size limit"));
    }
    directory
        .validate_file(name, &file)
        .map_err(|error| state_error("read binding", error))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| state_error("decode", error))
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
    Ok(format!("{kind}-{:x}.json", Sha256::digest(identity)))
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

fn write_record(
    root: &Path,
    directory: &JournalDirectory,
    generation: &str,
    mut record: RecoveryRecord,
) -> DbResult<()> {
    let name = record_name(&record)?;
    if let Some(previous) = read_json::<RecordDocument>(directory, &name)? {
        validate_document(&previous, generation)?;
        if record_name(&previous.record)? != name {
            return Err(state_error(
                "record",
                "stored identity does not match its filename",
            ));
        }
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
    directory
        .atomic_write(&name, |file| file.write_all(&bytes))
        .map_err(|error| state_error("record publication", error))?;
    note_pending_archive_paths(
        root,
        [format!(
            "{DIRECTORY}/{}/{name}",
            generation_directory(generation)?
        )],
    );
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

fn checkpoint_receipts(conn: &impl SyncQuery, writer: &mut BaselineWriter<'_>) -> DbResult<()> {
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
                writer.push(RecoveryRecord::Receipt(receipt))?;
            }
        }
    }
    Ok(())
}

fn checkpoint_contacts(conn: &impl SyncQuery, writer: &mut BaselineWriter<'_>) -> DbResult<()> {
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
            writer.push(RecoveryRecord::Contact(contact))?;
        }
    }
    Ok(())
}

fn verify_checkpoint_counts(conn: &impl SyncQuery, manifest: &Manifest) -> DbResult<()> {
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
    for (column, written) in [
        ("projects", manifest.projects),
        ("agents", manifest.agents),
        ("receipts", manifest.receipts),
        ("contacts", manifest.contacts),
    ] {
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
    checkpoint_receipts(conn, &mut writer)?;
    checkpoint_contacts(conn, &mut writer)?;
    verify_checkpoint_counts(conn, &writer.manifest)?;
    writer.manifest.baseline_sha256 = format!("{:x}", writer.digest.finalize());
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
    let directory = state.generation(generation, true)?;
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
    if let Some(previous) = active {
        // Record previous activation before changing active.json. An orphan
        // baseline created by an interrupted first publication has no such
        // marker and can safely be regenerated. The old active generation
        // remains usable if the attempted switch fails before publication.
        let prior_directory = state.generation(&previous.generation, false)?;
        let bytes = serde_json::to_vec(&previous)
            .map_err(|error| state_error("superseded manifest encode", error))?;
        prior_directory
            .atomic_write(SUPERSEDED_FILE, |file| file.write_all(&bytes))
            .map_err(|error| state_error("superseded generation publication", error))?;
        note_pending_archive_paths(
            root,
            [format!(
                "{DIRECTORY}/{}/{SUPERSEDED_FILE}",
                generation_directory(&previous.generation)?
            )],
        );
    }
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

/// Publish the initial consistent baseline unless this generation already has
/// one. The generation's incremental snapshots remain authoritative thereafter.
pub fn ensure_checkpoint(conn: &impl SyncQuery, root: &Path) -> DbResult<Manifest> {
    let state = LockedState::open(root, true)?;
    let generation = ensure_generation(conn)?;
    let manifest = checkpoint_locked(&state, conn, root, &generation)?;
    // Startup verifies pre-existing metadata too. Incremental publishers call
    // checkpoint_locked directly and never rescan history on the hot path.
    if visit_records(root, |_| Ok(()))?.as_ref() != Some(&manifest) {
        return Err(state_error(
            "startup checkpoint",
            "completed generation disappeared during validation",
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
        if (name.starts_with("receipt-") || name.starts_with("contact-")) && name.ends_with(".json")
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

/// Bind a Git commit's manifest and baseline to the same active generation.
/// The storage publisher holds its own archive locks outside this function;
/// state writers only enqueue paths and never call back into Git while locked.
pub fn with_archive_publication<T>(
    root: &Path,
    pending: &[String],
    publish: impl FnOnce(&[String]) -> DbResult<T>,
) -> DbResult<T> {
    let state = LockedState::open(root, false)?;
    let mut paths = BTreeSet::new();
    for path in pending {
        let mut components = Path::new(path).components();
        if components.next()
            != Some(std::path::Component::Normal(std::ffi::OsStr::new(
                DIRECTORY,
            )))
            || !components.all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            return Err(state_error(
                "Git publication",
                "pending path escapes recovery-state",
            ));
        }
        paths.insert(path.clone());
    }
    if let Some(manifest) = read_manifest(&state.directory)? {
        let generation = state.generation(&manifest.generation, false)?;
        generation
            .open_file(BASELINE_FILE, JournalFileMode::Read)
            .map_err(|error| state_error("Git baseline", error))?;
        paths.insert(format!("{DIRECTORY}/{MANIFEST_FILE}"));
        paths.insert(format!(
            "{DIRECTORY}/{}/{BASELINE_FILE}",
            generation_directory(&manifest.generation)?
        ));
    }
    let result = publish(&paths.into_iter().collect::<Vec<_>>())?;
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
                        root,
                        &directory,
                        &generation,
                        RecoveryRecord::Receipt(receipt(&row, shared)?),
                    )?;
                }
            }
        }
        Ok(())
    })
    .map_err(|error| state_error("receipt snapshot", error))?;
    state.validate()
}

/// Reconcile all committed receipts for one project's bulk mark-read operation.
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
                        root,
                        &directory,
                        &generation,
                        RecoveryRecord::Receipt(receipt),
                    )?;
                }
            }
        }
        Ok(())
    })
    .map_err(|error| state_error("bulk receipt snapshot", error))?;
    state.validate()
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
                root,
                &directory,
                &generation,
                RecoveryRecord::Contact(contact(row)?),
            )?;
        }
        Ok(())
    })
    .map_err(|error| state_error("contact snapshot", error))?;
    state.validate()
}

fn checked_baseline(generation: &JournalDirectory, manifest: &Manifest) -> DbResult<File> {
    let mut baseline = generation
        .open_file(BASELINE_FILE, JournalFileMode::Read)
        .map_err(|error| state_error("baseline open", error))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let size = baseline
            .read(&mut buffer)
            .map_err(|error| state_error("baseline checksum", error))?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    if format!("{:x}", digest.finalize()) != manifest.baseline_sha256 {
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
    generation: &JournalDirectory,
    manifest: &Manifest,
    visit: &mut impl FnMut(RecoveryRecord) -> DbResult<()>,
) -> DbResult<()> {
    for entry in std::fs::read_dir(generation.directory_path())
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
        let document: RecordDocument = read_json(generation, name)?.ok_or_else(|| {
            state_error(
                "record inventory",
                "recovery record disappeared during inspection",
            )
        })?;
        validate_document(&document, &manifest.generation)?;
        if record_name(&document.record)? != name {
            return Err(state_error(
                "record identity",
                "record does not match its filename",
            ));
        }
        visit(document.record)?;
    }
    Ok(())
}

/// Visit the checked baseline first, followed by per-key updates from its one
/// active generation. Missing legacy metadata is `None`; malformed or incomplete
/// metadata is an error and must never be promoted as a successful recovery.
pub fn visit_records(
    root: &Path,
    mut visit: impl FnMut(RecoveryRecord) -> DbResult<()>,
) -> DbResult<Option<Manifest>> {
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
    visit_incremental_records(&generation, &manifest, &mut visit)?;
    if read_manifest(&directory)?.as_ref() != Some(&manifest) {
        return Err(state_error(
            "manifest",
            "active generation changed while reading recovery state",
        ));
    }
    Ok(Some(manifest))
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
            Ok(())
        })
        .unwrap();
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
