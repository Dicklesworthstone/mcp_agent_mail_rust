//! Reconstruct a `SQLite` database from the Git archive.
//!
//! When the database file is corrupt and no healthy backup exists, this module
//! walks the per-project Git archive directories to recover:
//!
//! - **Projects** — from subdirectory names under `{storage_root}/projects/`
//!   plus optional `project.json` metadata for exact `human_key` recovery
//! - **Agents** — from `agents/{name}/profile.json` files
//! - **File reservations** — from `file_reservations/*.json` artifacts
//! - **Messages** — from `messages/{YYYY}/{MM}/*.md` files (JSON frontmatter)
//! - **Message recipients** — from the `to`, `cc`, `bcc` arrays in frontmatter
//!
//! Archive-only reconstruction will be missing:
//! - `read_ts` / `ack_ts` on `message_recipients` (no archive artifact for these)
//! - `agent_links` / contacts (handshake state not archived)
//! - `products` / `product_project_links` (not archived)
//!
//! Recovery flows that have a readable salvage database merge those DB-only rows
//! back into the reconstructed mailbox so contact and product-bus state is
//! preserved alongside the canonical archive-backed data.

use crate::error::{DbError, DbResult};
use crate::schema;
use serde::Serialize;
use sqlmodel_core::{Error as SqlError, Value};
use sqlmodel_schema::Migration;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

type DbConn = crate::CanonicalDbConn;

fn open_read_only_salvage_db(path: &Path) -> DbResult<DbConn> {
    let config = sqlmodel_sqlite::SqliteConfig::file(path.to_string_lossy().into_owned())
        .flags(sqlmodel_sqlite::OpenFlags::read_only());
    let conn = DbConn::open(&config).map_err(|e| {
        DbError::Sqlite(format!(
            "reconstruct salvage: cannot open source {} read-only: {e}",
            path.display()
        ))
    })?;
    conn.execute_raw("PRAGMA query_only = ON;").map_err(|e| {
        DbError::Sqlite(format!(
            "reconstruct salvage: cannot enforce query-only source {}: {e}",
            path.display()
        ))
    })?;
    Ok(conn)
}

/// Per-artifact size cap for archive reads during reconstruction (64 MiB).
///
/// Archive artifacts are read fully into memory; without a cap a single
/// oversized file (a multi-GB message body, a crafted `profile.json`, …) OOMs
/// the reconstruct path — which auto-runs on server-startup self-heal. The cap
/// is generous relative to any legitimate mailbox artifact.
const MAX_ARCHIVE_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

/// Read an archive text artifact with a bounded-memory cap (see
/// [`MAX_ARCHIVE_ARTIFACT_BYTES`]). Returns an `InvalidData` error if the file
/// exceeds the cap, which each call site already handles as a skippable read
/// failure (so an oversized artifact is logged/counted rather than OOMing).
fn read_archive_text_capped(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut buf = String::new();
    let read = file
        .by_ref()
        .take(MAX_ARCHIVE_ARTIFACT_BYTES + 1)
        .read_to_string(&mut buf)?;
    if read as u64 > MAX_ARCHIVE_ARTIFACT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "archive artifact exceeds {MAX_ARCHIVE_ARTIFACT_BYTES} byte cap: {}",
                path.display()
            ),
        ));
    }
    Ok(buf)
}

#[cfg(test)]
type SqliteDbConn = crate::CanonicalDbConn;

#[cfg(test)]
static FAIL_SALVAGE_MERGE_AFTER_PROJECTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static FAIL_SALVAGE_QUERY_MESSAGES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

const DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT: usize = 5;
const MALFORMED_ATTACHMENTS_SENTINEL: &str = "[malformed-attachments-json]";
const MALFORMED_RECIPIENTS_SENTINEL: &str = "[malformed-recipients-json]";
const VALID_RECONSTRUCTED_ATTACHMENTS_POLICIES: &[&str] = &["auto", "inline", "file", "none"];
const VALID_RECONSTRUCTED_CONTACT_POLICIES: &[&str] =
    &["open", "auto", "contacts_only", "block_all"];

fn trim_sql_identifier(token: &str) -> &str {
    token.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '[' | ']' | ';'))
}

fn parse_alter_table_add_column(sql: &str) -> Option<(String, String)> {
    let tokens: Vec<&str> = sql.split_whitespace().collect();
    if tokens.len() < 5
        || !tokens[0].eq_ignore_ascii_case("alter")
        || !tokens[1].eq_ignore_ascii_case("table")
        || !tokens[3].eq_ignore_ascii_case("add")
    {
        return None;
    }

    let table = trim_sql_identifier(tokens[2]);
    if table.is_empty() {
        return None;
    }

    let column_idx = if tokens
        .get(4)
        .is_some_and(|token| token.eq_ignore_ascii_case("column"))
    {
        5
    } else {
        4
    };
    let column = trim_sql_identifier(tokens.get(column_idx)?);
    if column.is_empty() {
        return None;
    }

    Some((table.to_string(), column.to_string()))
}

fn reconstruct_migration_preflight_already_satisfied(
    conn: &DbConn,
    migration: &Migration,
) -> DbResult<bool> {
    let Some((table, column)) = parse_alter_table_add_column(&migration.up) else {
        return Ok(false);
    };
    Ok(table_columns(conn, &table)?.contains(&column))
}

fn apply_snapshot_migrations(
    conn: &DbConn,
    migrations: Vec<Migration>,
    phase: &str,
) -> DbResult<()> {
    conn.execute_raw(&format!(
        "CREATE TABLE IF NOT EXISTS {} (\
            id TEXT PRIMARY KEY ON CONFLICT IGNORE,\
            description TEXT NOT NULL,\
            applied_at INTEGER NOT NULL\
        )",
        schema::MIGRATIONS_TABLE_NAME,
    ))
    .map_err(|e| DbError::Sqlite(format!("reconstruct: migrations table: {e}")))?;

    let applied_rows = conn
        .query_sync(
            &format!("SELECT id FROM {}", schema::MIGRATIONS_TABLE_NAME),
            &[],
        )
        .map_err(|e| DbError::Sqlite(format!("reconstruct: read migration set: {e}")))?;
    let mut applied_ids = applied_rows
        .into_iter()
        .filter_map(|row| row.get_named::<String>("id").ok())
        .collect::<HashSet<_>>();

    for migration in migrations {
        if applied_ids.contains(&migration.id) {
            continue;
        }

        let already_satisfied =
            reconstruct_migration_preflight_already_satisfied(conn, &migration)?;
        if !already_satisfied {
            conn.execute_raw(&migration.up).map_err(|e| {
                DbError::Sqlite(format!(
                    "reconstruct: apply {phase} migration {} ({}): {e}",
                    migration.id, migration.description
                ))
            })?;
        }

        conn.execute_sync(
            &format!(
                "INSERT OR IGNORE INTO {} (id, description, applied_at) VALUES (?, ?, ?)",
                schema::MIGRATIONS_TABLE_NAME,
            ),
            &[
                Value::Text(migration.id.clone()),
                Value::Text(migration.description.clone()),
                Value::BigInt(crate::now_micros()),
            ],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct: record {phase} migration {}: {e}",
                migration.id
            ))
        })?;
        applied_ids.insert(migration.id.clone());
    }

    Ok(())
}

fn apply_base_migrations_after_snapshot(conn: &DbConn) -> DbResult<()> {
    apply_snapshot_migrations(conn, schema::schema_migrations_base(), "base")
}

/// Recreate the ATC schema family in the dedicated `atc.sqlite3` sidecar.
///
/// ATC telemetry is isolated into a sidecar DB next to the primary mailbox DB
/// (br-bvq1x.11.7) and MUST NOT live in the primary mailbox DB — pool init drops
/// any `atc_*` it finds there, and `reconstruct_with_agent_profile` asserts the
/// rebuilt primary DB has no `atc_*` tables. `schema_migrations_base()` omits the
/// ATC family (`atc_experiences` and its v17 ALTERs, `atc_leader_lease`,
/// `atc_rollup_snapshots`, …) because FrankenConnection can't host it; at runtime
/// the canonical follow-up runner applies that family to the sidecar. Since
/// reconstruction rebuilds the primary DB, recreate the sidecar's ATC schema here
/// too — otherwise the ATC subsystem has no tables to write to after recovery (the
/// `v17` schema-surface regression). The sidecar opens through canonical SQLite
/// (which can host the family); the migrations are ordered (`atc_experiences`
/// created before its ALTERs) and the per-migration preflight skips anything
/// already present. The tables come up empty (ATC state isn't archived), the
/// correct post-recovery state. A `:memory:` target keeps ATC co-located, so there
/// is no sidecar to build.
pub(crate) fn recreate_atc_sidecar_schema(primary_db_path: &Path) -> DbResult<()> {
    let Some(primary) = primary_db_path.to_str() else {
        return Ok(());
    };
    if primary == ":memory:" {
        return Ok(());
    }
    let sidecar_path = crate::pool::atc_sidecar_sqlite_path(primary);
    // Refuse a symlinked sidecar target, exactly like the primary reconstruct
    // target and the salvage source: recovery must never write through a
    // pre-planted link.
    crate::pool::validate_sqlite_target_path(Path::new(&sidecar_path), "reconstruct ATC sidecar")
        .map_err(|error| DbError::Sqlite(format!("reconstruct: {error}")))?;
    match apply_atc_sidecar_schema(&sidecar_path) {
        Ok(()) => Ok(()),
        Err(first_error) if Path::new(&sidecar_path).exists() => {
            // A pre-existing sidecar that cannot be opened/migrated (the disk
            // incident that corrupted the primary DB usually hits its
            // same-directory sibling too) must NOT wedge recovery of the
            // PRIMARY mailbox: ATC telemetry is droppable by contract, while a
            // fatal abort here blocks every reconstruct retry until a human
            // intervenes. Quarantine the unusable sidecar by rename (never
            // delete) and rebuild a fresh one; only a failure on the fresh
            // file — a genuine environment problem — stays fatal.
            let quarantine_path = format!("{sidecar_path}.quarantined-{}", crate::now_micros());
            std::fs::rename(&sidecar_path, &quarantine_path).map_err(|rename_error| {
                DbError::Sqlite(format!(
                    "reconstruct: ATC sidecar {sidecar_path} is unusable ({first_error}) and \
                     could not be quarantined to {quarantine_path}: {rename_error}"
                ))
            })?;
            tracing::warn!(
                sidecar = %sidecar_path,
                quarantine = %quarantine_path,
                error = %first_error,
                "reconstruct: quarantined unusable ATC sidecar; rebuilding a fresh one"
            );
            apply_atc_sidecar_schema(&sidecar_path).map_err(|retry_error| {
                DbError::Sqlite(format!(
                    "reconstruct: rebuild ATC sidecar {sidecar_path} after quarantining the \
                     unusable one at {quarantine_path}: {retry_error}"
                ))
            })
        }
        // No sidecar file on disk and creation still failed: a real environment
        // problem (permissions, disk). A recovery that silently half-succeeds is
        // worse than one that fails loudly, so this stays fatal.
        Err(error) => Err(error),
    }
}

/// Open (creating if needed) the ATC sidecar at `sidecar_path` and apply the
/// canonical ATC follow-up migration set.
///
/// A sidecar created here gets the same posture as one created by the live
/// runtime (`ensure_file_backed_atc_pool_initialized`): WAL journal mode via
/// `PRAGMA_DB_INIT_SQL` and private 0600 permissions — it carries project keys,
/// subjects, and evidence summaries just like `storage.sqlite3`.
fn apply_atc_sidecar_schema(sidecar_path: &str) -> DbResult<()> {
    let preexisting = Path::new(sidecar_path).exists();
    let sidecar = DbConn::open_file(sidecar_path).map_err(|error| {
        DbError::Sqlite(format!(
            "reconstruct: open ATC sidecar {sidecar_path}: {error}"
        ))
    })?;
    let _ = sidecar.execute_raw(schema::PRAGMA_CONN_SETTINGS_SQL);
    if !preexisting {
        // journal_mode is DB-wide and intentionally omitted from
        // PRAGMA_CONN_SETTINGS_SQL; apply it once at sidecar creation, exactly
        // like the runtime creation path.
        sidecar
            .execute_raw(schema::PRAGMA_DB_INIT_SQL)
            .map_err(|error| {
                DbError::Sqlite(format!(
                    "reconstruct: set ATC sidecar db pragmas for {sidecar_path}: {error}"
                ))
            })?;
        // Best-effort 0600, matching the runtime creation path: a chmod failure
        // must not block recovery.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(error) =
                std::fs::set_permissions(sidecar_path, std::fs::Permissions::from_mode(0o600))
            {
                tracing::warn!(
                    path = %sidecar_path,
                    error = %error,
                    "reconstruct: failed to restrict ATC sidecar permissions to 0600"
                );
            }
        }
    }
    apply_snapshot_migrations(
        &sidecar,
        schema::schema_migrations_atc_runtime_canonical_followup(),
        "atc-sidecar-followup",
    )
}

/// Statistics returned after a reconstruction attempt.
#[derive(Debug, Clone, Default)]
pub struct ReconstructStats {
    /// Number of projects discovered and inserted.
    pub projects: usize,
    /// Number of agents discovered and inserted.
    pub agents: usize,
    /// Number of messages recovered from archive files.
    pub messages: usize,
    /// Number of message-recipient rows inserted.
    pub recipients: usize,
    /// Number of duplicate canonical archive files skipped because their
    /// positive frontmatter `id` had already been recovered within the same
    /// project.
    pub duplicate_canonical_message_files: usize,
    /// Number of distinct logical message ids represented by the skipped
    /// duplicate canonical archive files.
    pub duplicate_canonical_message_ids: usize,
    /// Number of messages re-inserted under a generated DB id because their
    /// canonical frontmatter id collided with a message from a *different*
    /// project. These are preserved (not skipped) to avoid cross-project
    /// data loss.
    pub cross_project_canonical_collisions: usize,
    /// Number of projects recovered only from a salvaged database.
    pub salvaged_projects: usize,
    /// Number of agents recovered only from a salvaged database.
    pub salvaged_agents: usize,
    /// Number of messages recovered only from a salvaged database.
    pub salvaged_messages: usize,
    /// Number of salvaged messages whose source-local numeric id collided
    /// with an archive message from another project and was remapped.
    pub salvaged_message_id_remaps: usize,
    /// Number of recipient rows inserted or state rows updated from a salvaged database.
    pub salvaged_recipients: usize,
    /// Number of reservation rows inserted or state rows updated from a salvaged database.
    pub salvaged_reservations: usize,
    /// Number of terminal reservation-release ledger rows restored from a salvaged database.
    pub salvaged_reservation_releases: usize,
    /// Number of ATC rollup rows restored from a salvaged database.
    pub rollups_salvaged: usize,
    /// Number of archive files that failed to parse (skipped).
    pub parse_errors: usize,
    /// Human-readable warnings collected during reconstruction.
    pub warnings: Vec<String>,
    duplicate_canonical_id_set: BTreeSet<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct MailboxProjectIdentity {
    pub slug: Option<String>,
    pub human_key: Option<String>,
}

impl MailboxProjectIdentity {
    #[must_use]
    pub fn from_parts(
        slug: Option<String>,
        human_key: Option<String>,
        fallback_slug: Option<String>,
    ) -> Option<Self> {
        let slug = normalize_inventory_identity_text(slug).or_else(|| {
            fallback_slug.and_then(|value| normalize_inventory_identity_text(Some(value)))
        });
        let human_key = normalize_inventory_identity_text(human_key);
        if slug.is_none() && human_key.is_none() {
            None
        } else {
            Some(Self { slug, human_key })
        }
    }

    fn exact_matches(&self, other: &Self) -> bool {
        let slug_match = self
            .slug
            .as_deref()
            .zip(other.slug.as_deref())
            .map(|(left, right)| left == right);
        let human_key_match = self
            .human_key
            .as_deref()
            .zip(other.human_key.as_deref())
            .map(|(left, right)| left == right);

        if matches!(slug_match, Some(false)) || matches!(human_key_match, Some(false)) {
            return false;
        }

        matches!(slug_match, Some(true)) || matches!(human_key_match, Some(true))
    }

    #[must_use]
    pub fn display_label(&self) -> String {
        match (self.slug.as_deref(), self.human_key.as_deref()) {
            (Some(slug), Some(human_key)) => format!("{slug} ({human_key})"),
            (Some(slug), None) => slug.to_string(),
            (None, Some(human_key)) => human_key.to_string(),
            (None, None) => "<unknown project>".to_string(),
        }
    }
}

/// Lightweight canonical archive inventory used for drift detection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveMessageInventory {
    /// Number of canonical archive project directories under `projects/`.
    pub projects: usize,
    /// Number of canonical agent profiles under `projects/*/agents/*/profile.json`.
    pub agents: usize,
    /// Canonical project identities discovered from `project.json` metadata or
    /// directory fallbacks when metadata is absent.
    pub project_identities: BTreeSet<MailboxProjectIdentity>,
    /// Number of canonical archive files under `messages/YYYY/MM/*.md`.
    pub canonical_message_files: usize,
    /// Number of unique positive message ids represented by those files.
    pub unique_message_ids: usize,
    /// Number of duplicate canonical archive files skipped by id.
    pub duplicate_canonical_message_files: usize,
    /// Number of distinct ids represented by the duplicate files.
    pub duplicate_canonical_message_ids: usize,
    /// Largest positive canonical message id observed in the archive.
    pub latest_message_id: Option<i64>,
    /// Number of canonical message files that failed JSON frontmatter parsing.
    pub parse_errors: usize,
}

impl ArchiveMessageInventory {
    fn record_message_id(&mut self, message_id: i64, seen_ids: &mut BTreeSet<i64>) {
        self.latest_message_id = Some(
            self.latest_message_id
                .map_or(message_id, |current| current.max(message_id)),
        );
        if seen_ids.insert(message_id) {
            self.unique_message_ids += 1;
        } else {
            self.duplicate_canonical_message_files += 1;
        }
    }
}

impl ReconstructStats {
    fn record_duplicate_canonical_message(&mut self, message_id: i64, file_path: &Path) {
        self.duplicate_canonical_message_files += 1;
        if self.duplicate_canonical_id_set.insert(message_id) {
            self.duplicate_canonical_message_ids += 1;
        }
        if self.duplicate_canonical_message_files <= DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT {
            self.warnings.push(format!(
                "Duplicate canonical message id {message_id} in {}; keeping the first archive artifact and skipping the duplicate",
                file_path.display()
            ));
        }
    }

    fn record_cross_project_canonical_collision(
        &mut self,
        message_id: i64,
        existing_project_id: i64,
        new_project_id: i64,
        file_path: &Path,
    ) {
        self.cross_project_canonical_collisions += 1;
        if self.cross_project_canonical_collisions <= DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT {
            self.warnings.push(format!(
                "Cross-project canonical message id {message_id} collision in {}: \
                 existing message belongs to project_id {existing_project_id}, \
                 new archive artifact belongs to project_id {new_project_id}; \
                 inserting under a generated DB id to avoid data loss",
                file_path.display()
            ));
        }
    }

    fn finalize_duplicate_warnings(&mut self) {
        if self.duplicate_canonical_message_files <= DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT {
            return;
        }

        let sample_ids = self
            .duplicate_canonical_id_set
            .iter()
            .take(DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT)
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        self.warnings.push(format!(
            "Skipped {} duplicate canonical message file(s) across {} logical message id(s); sample ids: {}",
            self.duplicate_canonical_message_files,
            self.duplicate_canonical_message_ids,
            sample_ids
        ));
    }

    fn finalize_cross_project_canonical_collision_warnings(&mut self) {
        if self.cross_project_canonical_collisions <= DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT {
            return;
        }
        self.warnings.push(format!(
            "Preserved {} cross-project canonical id collision(s) under generated DB ids; only the first {} were itemized in warnings above",
            self.cross_project_canonical_collisions,
            DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT
        ));
    }
}

impl std::fmt::Display for ReconstructStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reconstructed {} projects, {} agents, {} messages ({} recipients), {} parse errors",
            self.projects, self.agents, self.messages, self.recipients, self.parse_errors
        )?;
        if self.duplicate_canonical_message_files > 0 {
            write!(
                f,
                "; skipped {} duplicate canonical file(s) across {} message id(s)",
                self.duplicate_canonical_message_files, self.duplicate_canonical_message_ids
            )?;
        }
        if self.cross_project_canonical_collisions > 0 {
            write!(
                f,
                "; preserved {} cross-project canonical id collision(s) under generated DB ids",
                self.cross_project_canonical_collisions
            )?;
        }
        if self.salvaged_projects > 0
            || self.salvaged_agents > 0
            || self.salvaged_messages > 0
            || self.salvaged_message_id_remaps > 0
            || self.salvaged_recipients > 0
            || self.salvaged_reservations > 0
            || self.salvaged_reservation_releases > 0
            || self.rollups_salvaged > 0
        {
            write!(
                f,
                "; salvaged {} projects, {} agents, {} messages ({} numeric-id remaps, {} recipients/state updates, {} reservations, {} reservation releases, {} rollups)",
                self.salvaged_projects,
                self.salvaged_agents,
                self.salvaged_messages,
                self.salvaged_message_id_remaps,
                self.salvaged_recipients,
                self.salvaged_reservations,
                self.salvaged_reservation_releases,
                self.rollups_salvaged
            )?;
        }
        Ok(())
    }
}

fn normalize_inventory_identity_text(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn project_identity_match_tokens(identity: &MailboxProjectIdentity) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    if let Some(slug) = identity
        .slug
        .as_deref()
        .and_then(normalized_project_match_token)
    {
        tokens.insert(slug);
    }
    if let Some(basename) = identity
        .human_key
        .as_deref()
        .and_then(project_basename_token_for_human_key)
    {
        tokens.insert(basename);
    }
    tokens
}

fn project_identity_token_candidates<'a>(
    archive_identity: &MailboxProjectIdentity,
    db_identities: &'a BTreeSet<MailboxProjectIdentity>,
) -> Vec<&'a MailboxProjectIdentity> {
    let archive_tokens = project_identity_match_tokens(archive_identity);
    if archive_tokens.is_empty() {
        return Vec::new();
    }

    db_identities
        .iter()
        .filter(|db_identity| {
            (archive_identity.human_key.is_none() || db_identity.human_key.is_none())
                && !archive_tokens.is_disjoint(&project_identity_match_tokens(db_identity))
        })
        .collect()
}

#[must_use]
pub fn mailbox_project_identity_matches_db(
    archive_identity: &MailboxProjectIdentity,
    db_identities: &BTreeSet<MailboxProjectIdentity>,
) -> bool {
    let exact_match_count = db_identities
        .iter()
        .filter(|db_identity| archive_identity.exact_matches(db_identity));
    match exact_match_count.take(2).count() {
        1 => return true,
        2 => return false,
        0 => {}
        _ => unreachable!("take(2) limits the exact match count"),
    }

    project_identity_token_candidates(archive_identity, db_identities).len() == 1
}

#[must_use]
pub fn archive_missing_project_identities(
    archive: &ArchiveMessageInventory,
    db_identities: &BTreeSet<MailboxProjectIdentity>,
) -> Vec<String> {
    archive
        .project_identities
        .iter()
        .filter(|archive_identity| {
            !mailbox_project_identity_matches_db(archive_identity, db_identities)
        })
        .map(MailboxProjectIdentity::display_label)
        .collect()
}

// ============================================================================
// Archive drift report — per-message-ID evidence for forensic bundles
// ============================================================================

/// A project identity seen in one source but not the other, or present in both
/// but with conflicting slug/human_key values.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectIdentityMismatch {
    /// The identity as seen in the archive (if present).
    pub archive: Option<MailboxProjectIdentity>,
    /// The identity as seen in the database (if present).
    pub db: Option<MailboxProjectIdentity>,
    /// Human-readable description of the mismatch.
    pub reason: String,
}

/// Per-message-ID drift evidence captured before any reconstruct or recovery
/// mutation, so that callers can reason about exactly which messages the archive
/// has that the DB does not, and vice versa.
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveDriftReport {
    /// Schema marker for downstream tooling.
    pub schema: ArchiveDriftReportSchema,
    /// Microsecond timestamp when the report was generated.
    pub captured_at_us: i64,
    /// Total unique message IDs in the archive.
    pub archive_message_count: usize,
    /// Total message IDs in the database.
    pub db_message_count: usize,
    /// Messages present in both archive and DB.
    pub shared_message_count: usize,
    /// Message IDs present in the archive but absent from the DB.
    pub archive_only_ids: BTreeSet<i64>,
    /// Message IDs present in the DB but absent from the archive.
    pub db_only_ids: BTreeSet<i64>,
    /// Project identity mismatches between archive and DB.
    pub identity_mismatches: Vec<ProjectIdentityMismatch>,
    /// Archive inventory counts (for cross-reference with existing drift checks).
    pub archive_projects: usize,
    /// DB project count.
    pub db_projects: usize,
    /// Archive agent count.
    pub archive_agents: usize,
    /// DB agent count.
    pub db_agents: usize,
    /// Largest message ID in the archive.
    pub archive_latest_message_id: Option<i64>,
    /// Largest message ID in the DB.
    pub db_max_message_id: i64,
    /// Warnings or errors encountered while building the report.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveDriftReportSchema {
    pub name: &'static str,
    pub major: u32,
    pub minor: u32,
}

impl Default for ArchiveDriftReportSchema {
    fn default() -> Self {
        Self {
            name: "mcp-agent-mail-archive-drift-report",
            major: 1,
            minor: 0,
        }
    }
}

impl ArchiveDriftReport {
    /// True when there is any per-ID drift (archive-only or db-only messages).
    #[must_use]
    pub fn has_message_drift(&self) -> bool {
        !self.archive_only_ids.is_empty() || !self.db_only_ids.is_empty()
    }

    /// True when there are project identity mismatches.
    #[must_use]
    pub fn has_identity_drift(&self) -> bool {
        !self.identity_mismatches.is_empty()
    }

    /// True when there is any drift at all.
    #[must_use]
    pub fn has_any_drift(&self) -> bool {
        self.has_message_drift() || self.has_identity_drift()
    }
}

/// Walk the archive and return the full set of positive message IDs found in
/// canonical message files (frontmatter `"id"` fields).
///
/// This is a heavier variant of [`scan_archive_message_inventory`] that retains
/// the actual ID set instead of only counting unique entries.
#[must_use]
pub fn scan_archive_message_ids(storage_root: &Path) -> (BTreeSet<i64>, usize) {
    let mut ids = BTreeSet::new();
    let mut parse_errors: usize = 0;
    let projects_dir = storage_root.join("projects");
    if !is_real_directory(&projects_dir) {
        return (ids, parse_errors);
    }

    let Ok(project_entries) = std::fs::read_dir(&projects_dir) else {
        return (ids, parse_errors);
    };

    for entry in project_entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        collect_project_archive_message_ids(&path.join("messages"), &mut ids, &mut parse_errors);
    }

    (ids, parse_errors)
}

fn collect_project_archive_message_ids(
    messages_dir: &Path,
    ids: &mut BTreeSet<i64>,
    parse_errors: &mut usize,
) {
    if !is_real_directory(messages_dir) {
        return;
    }

    let Ok(year_entries) = std::fs::read_dir(messages_dir) else {
        return;
    };

    for year_entry in year_entries.flatten() {
        let year_path = year_entry.path();
        let Ok(year_type) = year_entry.file_type() else {
            continue;
        };
        if !year_type.is_dir() || year_type.is_symlink() {
            continue;
        }
        let Some(year_name) = year_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if year_name.len() != 4 || !year_name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }

        let Ok(month_entries) = std::fs::read_dir(&year_path) else {
            continue;
        };
        for month_entry in month_entries.flatten() {
            let month_path = month_entry.path();
            let Ok(month_type) = month_entry.file_type() else {
                continue;
            };
            if !month_type.is_dir() || month_type.is_symlink() {
                continue;
            }
            let Some(month_name) = month_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if month_name.len() != 2 || !month_name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }

            let Ok(file_entries) = std::fs::read_dir(&month_path) else {
                continue;
            };
            for file_entry in file_entries.flatten() {
                let file_path = file_entry.path();
                let Ok(file_type) = file_entry.file_type() else {
                    continue;
                };
                if !file_type.is_file()
                    || file_type.is_symlink()
                    || file_path.extension().is_none_or(|ext| ext != "md")
                {
                    continue;
                }
                match scan_archive_message_id(&file_path) {
                    Ok(Some(message_id)) => {
                        ids.insert(message_id);
                    }
                    Ok(None) => {}
                    Err(_) => *parse_errors += 1,
                }
            }
        }
    }
}

/// Query the database for all message IDs.
#[allow(clippy::result_large_err)]
pub fn collect_db_message_ids(db_path: &Path) -> Result<BTreeSet<i64>, SqlError> {
    if db_path.as_os_str() == ":memory:" {
        return Err(SqlError::Custom(
            "DB message-id inventory is unavailable for in-memory databases".to_string(),
        ));
    }

    // `DbConn::open_file` opens SQLite with `SQLITE_OPEN_CREATE`, which would
    // silently materialize an empty DB stub for a missing mailbox.  This is
    // a read-only inventory probe used by `compute_archive_drift_report` and
    // `scan_archive_anomalies_with_db`, so refuse cleanly rather than mutate
    // the filesystem for the caller. Reject symlinked paths as well: opening a
    // symlink with SQLite can create journals or WAL files next to the target.
    crate::pool::validate_sqlite_target_path(db_path, "DB message-id inventory target")
        .map_err(|error| SqlError::Custom(format!("collect_db_message_ids: {error}")))?;
    let metadata = match std::fs::symlink_metadata(db_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SqlError::Custom(format!(
                "collect_db_message_ids: database file not found at {}",
                db_path.display()
            )));
        }
        Err(error) => {
            return Err(SqlError::Custom(format!(
                "collect_db_message_ids: failed to inspect database file {}: {error}",
                db_path.display()
            )));
        }
    };
    if !metadata.file_type().is_file() {
        return Err(SqlError::Custom(format!(
            "collect_db_message_ids: refusing non-regular database file {}",
            db_path.display()
        )));
    }

    let db_str = db_path.to_string_lossy();
    let conn = DbConn::open_file(db_str.as_ref()).map_err(|e| {
        SqlError::Custom(format!(
            "collect_db_message_ids: cannot open {}: {e}",
            db_path.display()
        ))
    })?;
    // Check if messages table exists.
    let tables = conn.query_sync(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='messages'",
        &[],
    )?;
    if tables.is_empty() {
        return Ok(BTreeSet::new());
    }
    let rows = conn.query_sync("SELECT id FROM messages", &[])?;
    let mut ids = BTreeSet::new();
    for row in rows {
        if let Ok(id) = row.get_named::<i64>("id") {
            ids.insert(id);
        }
    }
    Ok(ids)
}

/// Compare project identities between archive and DB, returning mismatches.
fn compute_identity_mismatches(
    archive_identities: &BTreeSet<MailboxProjectIdentity>,
    db_identities: &BTreeSet<MailboxProjectIdentity>,
) -> Vec<ProjectIdentityMismatch> {
    let mut mismatches = Vec::new();

    // No archive project identities means there is no durable archive-side
    // identity state to compare against yet. Treating DB-only identities as
    // drift in that case creates false positives for empty/new mailboxes and
    // can incorrectly steer doctor flows toward reconstruction.
    if archive_identities.is_empty() {
        return mismatches;
    }

    // Archive identities not matched in DB.
    for archive_id in archive_identities {
        if !mailbox_project_identity_matches_db(archive_id, db_identities) {
            // Check if there's a partial match (token overlap but not exact).
            let candidates = project_identity_token_candidates(archive_id, db_identities);
            if candidates.is_empty() {
                mismatches.push(ProjectIdentityMismatch {
                    archive: Some(archive_id.clone()),
                    db: None,
                    reason: format!(
                        "Archive project {} has no matching DB identity",
                        archive_id.display_label()
                    ),
                });
            } else {
                for candidate in candidates {
                    mismatches.push(ProjectIdentityMismatch {
                        archive: Some(archive_id.clone()),
                        db: Some(candidate.clone()),
                        reason: format!(
                            "Archive project {} has ambiguous match with DB project {}",
                            archive_id.display_label(),
                            candidate.display_label()
                        ),
                    });
                }
            }
        }
    }

    // DB identities not found in archive (reverse check).
    for db_id in db_identities {
        let has_archive_match = archive_identities
            .iter()
            .any(|archive_id| archive_id.exact_matches(db_id));
        let has_token_match = !archive_identities.is_empty()
            && archive_identities.iter().any(|archive_id| {
                let archive_tokens = project_identity_match_tokens(archive_id);
                let db_tokens = project_identity_match_tokens(db_id);
                !archive_tokens.is_disjoint(&db_tokens)
            });
        if !has_archive_match && !has_token_match {
            mismatches.push(ProjectIdentityMismatch {
                archive: None,
                db: Some(db_id.clone()),
                reason: format!(
                    "DB project {} has no matching archive identity",
                    db_id.display_label()
                ),
            });
        }
    }

    mismatches
}

/// Compute a full archive drift report with per-message-ID evidence.
///
/// This captures the state of both the archive and the DB *before* any
/// reconstruct or recovery mutation, so the report reflects the pre-mutation
/// evidence that explains why drift exists.
///
/// # Errors
///
/// Returns an error only if the database cannot be opened or queried.
/// Archive scan failures are recorded as warnings, not errors.
pub fn compute_archive_drift_report(
    storage_root: &Path,
    db_path: &Path,
) -> DbResult<ArchiveDriftReport> {
    let mut warnings = Vec::new();
    let captured_at_us = crate::now_micros();

    // Scan archive for full message ID set.
    let (archive_ids, archive_parse_errors) = scan_archive_message_ids(storage_root);
    if archive_parse_errors > 0 {
        warnings.push(format!(
            "{archive_parse_errors} archive message file(s) failed to parse"
        ));
    }

    // Scan archive for inventory counts (projects, agents, identities).
    let archive_inventory = scan_archive_message_inventory(storage_root);

    if db_path.as_os_str() == ":memory:" {
        warnings.push("DB-side drift comparison skipped for in-memory database".to_string());
        return Ok(ArchiveDriftReport {
            schema: ArchiveDriftReportSchema::default(),
            captured_at_us,
            archive_message_count: archive_ids.len(),
            db_message_count: 0,
            shared_message_count: 0,
            archive_only_ids: BTreeSet::new(),
            db_only_ids: BTreeSet::new(),
            identity_mismatches: Vec::new(),
            archive_projects: archive_inventory.projects,
            db_projects: 0,
            archive_agents: archive_inventory.agents,
            db_agents: 0,
            archive_latest_message_id: archive_inventory.latest_message_id,
            db_max_message_id: 0,
            warnings,
        });
    }

    // Query DB for full message ID set.
    let db_ids = match collect_db_message_ids(db_path) {
        Ok(ids) => ids,
        Err(error) => {
            warnings.push(format!("Cannot read DB message IDs: {error}"));
            BTreeSet::new()
        }
    };

    // Query DB inventory for project/agent counts and identities.
    let (db_projects, db_agents, db_max_message_id, db_identities) =
        match crate::pool::inspect_mailbox_db_inventory(db_path) {
            Ok(inv) => (
                inv.projects,
                inv.agents,
                inv.max_message_id,
                inv.project_identities,
            ),
            Err(error) => {
                warnings.push(format!("Cannot read DB inventory: {error}"));
                (0, 0, 0, BTreeSet::new())
            }
        };

    // Compute set differences.
    let archive_only_ids: BTreeSet<i64> = archive_ids.difference(&db_ids).copied().collect();
    let db_only_ids: BTreeSet<i64> = db_ids.difference(&archive_ids).copied().collect();
    let shared_message_count = archive_ids.intersection(&db_ids).count();

    // Compute identity mismatches.
    let identity_mismatches =
        compute_identity_mismatches(&archive_inventory.project_identities, &db_identities);

    Ok(ArchiveDriftReport {
        schema: ArchiveDriftReportSchema::default(),
        captured_at_us,
        archive_message_count: archive_ids.len(),
        db_message_count: db_ids.len(),
        shared_message_count,
        archive_only_ids,
        db_only_ids,
        identity_mismatches,
        archive_projects: archive_inventory.projects,
        db_projects,
        archive_agents: archive_inventory.agents,
        db_agents,
        archive_latest_message_id: archive_inventory.latest_message_id,
        db_max_message_id,
        warnings,
    })
}

#[allow(clippy::result_large_err)]
pub fn collect_db_project_identities(
    conn: &crate::DbConn,
) -> Result<BTreeSet<MailboxProjectIdentity>, SqlError> {
    let mut project_identities = BTreeSet::new();
    let project_rows = conn.query_sync("SELECT slug, human_key FROM projects", &[])?;
    for row in project_rows {
        let slug = row.get_named::<String>("slug").ok();
        let human_key = row.get_named::<String>("human_key").ok();
        if let Some(identity) = MailboxProjectIdentity::from_parts(slug, human_key, None) {
            project_identities.insert(identity);
        }
    }
    Ok(project_identities)
}

/// Scan canonical archive message files without writing to SQLite.
#[must_use]
pub fn scan_archive_message_inventory(storage_root: &Path) -> ArchiveMessageInventory {
    let mut inventory = ArchiveMessageInventory::default();
    let projects_dir = storage_root.join("projects");
    if !is_real_directory(&projects_dir) {
        return inventory;
    }

    let Ok(project_entries) = std::fs::read_dir(&projects_dir) else {
        return inventory;
    };

    let mut seen_ids = BTreeSet::new();
    let mut duplicate_ids = BTreeSet::new();

    for entry in project_entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        inventory.projects += 1;
        if let Some(identity) = scan_archive_project_identity(&path) {
            inventory.project_identities.insert(identity);
        }
        inventory.agents += count_project_archive_agents(&path);
        scan_project_archive_message_inventory(
            &path.join("messages"),
            &mut inventory,
            &mut seen_ids,
            &mut duplicate_ids,
        );
    }

    inventory.duplicate_canonical_message_ids = duplicate_ids.len();
    inventory
}

fn scan_archive_project_identity(project_path: &Path) -> Option<MailboxProjectIdentity> {
    let fallback_slug = project_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string);
    let project_json = project_path.join("project.json");
    if let Ok(content) = read_archive_text_capped(&project_json)
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content)
    {
        return MailboxProjectIdentity::from_parts(
            parsed
                .get("slug")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            parsed
                .get("human_key")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            fallback_slug,
        );
    }

    MailboxProjectIdentity::from_parts(fallback_slug, None, None)
}

fn count_project_archive_agents(project_dir: &Path) -> usize {
    let agents_dir = project_dir.join("agents");
    if !is_real_directory(&agents_dir) {
        return 0;
    }

    let Ok(agent_entries) = std::fs::read_dir(&agents_dir) else {
        return 0;
    };

    agent_entries
        .flatten()
        .filter_map(|entry| {
            let Ok(file_type) = entry.file_type() else {
                return None;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                return None;
            }
            is_real_file(&entry.path().join("profile.json")).then_some(())
        })
        .count()
}

fn scan_project_archive_message_inventory(
    messages_dir: &Path,
    inventory: &mut ArchiveMessageInventory,
    seen_ids: &mut BTreeSet<i64>,
    duplicate_ids: &mut BTreeSet<i64>,
) {
    if !is_real_directory(messages_dir) {
        return;
    }

    let Ok(year_entries) = std::fs::read_dir(messages_dir) else {
        return;
    };

    for year_entry in year_entries.flatten() {
        let year_path = year_entry.path();
        let Ok(year_type) = year_entry.file_type() else {
            continue;
        };
        if !year_type.is_dir() || year_type.is_symlink() {
            continue;
        }
        let Some(year_name) = year_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if year_name.len() != 4 || !year_name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }

        let Ok(month_entries) = std::fs::read_dir(&year_path) else {
            continue;
        };
        for month_entry in month_entries.flatten() {
            let month_path = month_entry.path();
            let Ok(month_type) = month_entry.file_type() else {
                continue;
            };
            if !month_type.is_dir() || month_type.is_symlink() {
                continue;
            }
            let Some(month_name) = month_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if month_name.len() != 2 || !month_name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }

            let Ok(file_entries) = std::fs::read_dir(&month_path) else {
                continue;
            };
            for file_entry in file_entries.flatten() {
                let file_path = file_entry.path();
                let Ok(file_type) = file_entry.file_type() else {
                    continue;
                };
                if !file_type.is_file()
                    || file_type.is_symlink()
                    || file_path.extension().is_none_or(|ext| ext != "md")
                {
                    continue;
                }

                inventory.canonical_message_files += 1;
                match scan_archive_message_id(&file_path) {
                    Ok(Some(message_id)) => {
                        let existed = seen_ids.contains(&message_id);
                        inventory.record_message_id(message_id, seen_ids);
                        if existed {
                            duplicate_ids.insert(message_id);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => inventory.parse_errors += 1,
                }
            }
        }
    }
}

fn scan_archive_message_id(file_path: &Path) -> DbResult<Option<i64>> {
    let content = read_archive_text_capped(file_path)
        .map_err(|e| DbError::Sqlite(format!("read {}: {e}", file_path.display())))?;
    let Some(frontmatter) = extract_json_frontmatter(&content) else {
        return Ok(None);
    };
    let msg: serde_json::Value = serde_json::from_str(frontmatter)
        .map_err(|e| DbError::Sqlite(format!("bad JSON in {}: {e}", file_path.display())))?;
    Ok(msg
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0))
}

/// Reconstruct the database from the Git archive at `storage_root`.
///
/// When archive content exists, opens (or creates) a fresh `SQLite` database at
/// `db_path`, runs schema migrations, then walks the archive to recover data.
/// Empty archive roots are reported without creating a target database.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or if schema creation
/// fails. Individual archive files that fail to parse are skipped (counted
/// in `parse_errors`).
pub fn reconstruct_from_archive(db_path: &Path, storage_root: &Path) -> DbResult<ReconstructStats> {
    reconstruct_from_archive_impl(db_path, storage_root, false)
}

fn ensure_unoccupied_reconstruction_target_family(db_path: &Path) -> DbResult<()> {
    if db_path.as_os_str() == ":memory:" {
        return Ok(());
    }

    for path in std::iter::once(db_path.to_path_buf()).chain(
        ["-journal", "-wal", "-shm"]
            .into_iter()
            .map(|suffix| crate::pool::sqlite_path_with_suffix(db_path, suffix)),
    ) {
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(DbError::Sqlite(format!(
                "reconstruct: target family is already occupied at {}; reconstruction requires a fresh candidate path and never mutates an existing database generation",
                path.display()
            )));
        }
    }
    Ok(())
}

fn claim_fresh_reconstruction_target(db_path: &Path) -> DbResult<()> {
    if db_path.as_os_str() == ":memory:" {
        return Ok(());
    }

    ensure_unoccupied_reconstruction_target_family(db_path)?;

    // The low-level reconstruction API owns only fresh candidates. `create_new`
    // is the race-safe admission primitive: two builders can never both pass a
    // check-then-open window and replay into the same SQLite file.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(db_path)
        .map_err(|error| {
            DbError::Sqlite(format!(
                "reconstruct: failed to claim fresh candidate {}: {error}",
                db_path.display()
            ))
        })?;

    // Refuse sidecars that raced with candidate admission. The newly claimed
    // empty main file is intentionally retained as evidence; callers allocate
    // unique staging names and may quarantine failed candidates.
    for suffix in ["-journal", "-wal", "-shm"] {
        let sidecar = crate::pool::sqlite_path_with_suffix(db_path, suffix);
        if std::fs::symlink_metadata(&sidecar).is_ok() {
            return Err(DbError::Sqlite(format!(
                "reconstruct: target sidecar appeared during fresh-candidate admission at {}; refusing to share a SQLite generation",
                sidecar.display()
            )));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn reconstruct_from_archive_impl(
    db_path: &Path,
    storage_root: &Path,
    create_empty_target: bool,
) -> DbResult<ReconstructStats> {
    let mut stats = ReconstructStats::default();
    crate::pool::validate_sqlite_target_path(db_path, "reconstruct sqlite target")
        .map_err(|error| DbError::Sqlite(format!("reconstruct: {error}")))?;
    ensure_unoccupied_reconstruction_target_family(db_path)?;
    let projects_dir = storage_root.join("projects");
    let mut project_dirs: Vec<(String, PathBuf)> = Vec::new();
    if is_real_directory(storage_root) {
        if is_real_directory(&projects_dir) {
            if let Ok(entries) = std::fs::read_dir(&projects_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Ok(file_type) = entry.file_type() else {
                        continue;
                    };
                    if !file_type.is_dir() || file_type.is_symlink() {
                        continue;
                    }
                    let Some(slug) = path.file_name().and_then(|n| n.to_str()).map(String::from)
                    else {
                        continue;
                    };
                    project_dirs.push((slug, path));
                }
            }
        } else {
            stats.warnings.push(format!(
                "No projects directory found at {}",
                projects_dir.display()
            ));
            if !create_empty_target {
                return Ok(stats);
            }
        }
    } else {
        stats.warnings.push(format!(
            "Storage root {} is missing or not a real directory",
            storage_root.display()
        ));
        if !create_empty_target {
            return Ok(stats);
        }
    }
    project_dirs.sort_by(|a, b| a.0.cmp(&b.0));
    if project_dirs.is_empty() {
        stats.warnings.push(format!(
            "No project archives found under {}",
            projects_dir.display()
        ));
        if !create_empty_target {
            return Ok(stats);
        }
    }

    claim_fresh_reconstruction_target(db_path)?;

    let db_str = db_path.to_string_lossy();
    let conn = DbConn::open_file(db_str.as_ref()).map_err(|e| {
        DbError::Sqlite(format!(
            "reconstruct: cannot open {}: {e}",
            db_path.display()
        ))
    })?;

    // Apply base-mode PRAGMAs: DELETE journal (rollback) is safer for one-shot
    // reconstruction. WAL mode causes corruption when the runtime later opens
    // with different connection settings (e.g. FrankenConnection pool warmup).
    for pragma in schema::PRAGMA_DB_INIT_BASE_SQL.split(';') {
        let pragma = pragma.trim();
        if pragma.is_empty() {
            continue;
        }
        conn.execute_raw(&format!("{pragma};"))
            .map_err(|e| DbError::Sqlite(format!("reconstruct: pragma: {e}")))?;
    }
    conn.execute_raw("PRAGMA synchronous=NORMAL;")
        .map_err(|e| DbError::Sqlite(format!("reconstruct: synchronous: {e}")))?;
    conn.execute_raw("PRAGMA busy_timeout=60000;")
        .map_err(|e| DbError::Sqlite(format!("reconstruct: busy_timeout: {e}")))?;
    conn.execute_raw("BEGIN IMMEDIATE;")
        .map_err(|e| DbError::Sqlite(format!("reconstruct: begin transaction: {e}")))?;

    let rebuild_result = (|| -> DbResult<()> {
        // Lay down the latest base schema directly (base mode: no FTS5 virtual
        // tables, which FrankenConnection doesn't support). The base DDL already
        // reflects the current schema, so replaying schema-altering base
        // migrations on top of it can produce malformed tables under the
        // FrankenConnection path (for example duplicate columns in `agents`).
        let ddl = schema::init_schema_sql_base();
        for stmt in ddl.split(';') {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            conn.execute_raw(&format!("{stmt};"))
                .map_err(|e| DbError::Sqlite(format!("reconstruct: DDL: {e}")))?;
        }

        // Follow the snapshot DDL with a synchronous replay of base migrations.
        // The snapshot is intentionally ahead of many legacy mail tables, but it
        // can still lag later base-mode repairs and indexes. Replaying the base
        // migrations here keeps rebuilt DBs aligned with the current base schema
        // while preflighting `ALTER TABLE` additions so latest-schema columns are
        // not duplicated.
        apply_base_migrations_after_snapshot(&conn)?;

        // The ATC telemetry family is isolated in a fixed-name sibling
        // `atc.sqlite3`. Candidate construction must never touch it: a staged
        // candidate lives beside the current live database, so doing so would
        // mutate live state before promotion and make concurrent candidates
        // share a sidecar. The unified promotion boundary ensures the sidecar
        // schema only after this candidate is durably committed as live.

        // Clean up any FTS artifacts that may have been left by prior migrations.
        // This mirrors `schema::enforce_runtime_fts_cleanup`, but uses canonical
        // SQLite so reconstruction is not coupled to runtime connection type.
        let cleanup_sql = [
            "DROP TRIGGER IF EXISTS fts_messages_ai",
            "DROP TRIGGER IF EXISTS fts_messages_ad",
            "DROP TRIGGER IF EXISTS fts_messages_au",
            "DROP TRIGGER IF EXISTS messages_ai",
            "DROP TRIGGER IF EXISTS messages_ad",
            "DROP TRIGGER IF EXISTS messages_au",
            "DROP TRIGGER IF EXISTS agents_ai",
            "DROP TRIGGER IF EXISTS agents_ad",
            "DROP TRIGGER IF EXISTS agents_au",
            "DROP TRIGGER IF EXISTS projects_ai",
            "DROP TRIGGER IF EXISTS projects_ad",
            "DROP TRIGGER IF EXISTS projects_au",
            "DROP TABLE IF EXISTS fts_agents",
            "DROP TABLE IF EXISTS fts_projects",
            "DROP TABLE IF EXISTS fts_messages",
        ];
        for stmt in cleanup_sql {
            conn.execute_raw(stmt)
                .map_err(|e| DbError::Sqlite(format!("reconstruct: fts cleanup ({stmt}): {e}")))?;
        }

        // Maps for deduplication: ((project_id, name) → agent_id)
        let mut agent_ids: HashMap<(i64, String), i64> = HashMap::new();

        // Phase 1: Replay projects discovered before opening the target DB.
        for (slug, project_path) in &project_dirs {
            let now = crate::now_micros();
            let human_key = read_project_human_key(project_path, slug, &mut stats);

            conn.execute_sync(
                "INSERT OR IGNORE INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
                &[
                    Value::Text(slug.clone()),
                    Value::Text(human_key.clone()),
                    Value::BigInt(now),
                ],
            )
            .map_err(|e| DbError::Sqlite(format!("reconstruct: insert project {slug}: {e}")))?;

            let pid = query_last_insert_or_existing_id(&conn, "projects", "slug", slug)?;
            stats.projects += 1;

            // Phase 2: Discover agents for this project
            let agents_dir = project_path.join("agents");
            if is_real_directory(&agents_dir) {
                discover_agents(&conn, &agents_dir, pid, &mut agent_ids, &mut stats)?;
            }

            // Phase 2b: Recover archived file reservations so robot/status reads can
            // rebuild the same project-scoped lease view from the archive alone.
            let reservations_dir = project_path.join("file_reservations");
            if is_real_directory(&reservations_dir) {
                discover_file_reservations(
                    &conn,
                    &reservations_dir,
                    pid,
                    &mut agent_ids,
                    &mut stats,
                )?;
            }

            // Phase 3: Discover messages for this project
            let messages_dir = project_path.join("messages");
            if is_real_directory(&messages_dir) {
                discover_messages(&conn, &messages_dir, pid, slug, &mut agent_ids, &mut stats)?;
            }
        }

        // ATC telemetry now lives in a dedicated sidecar DB (atc.sqlite3) that
        // is NOT part of the Git archive (br-bvq1x.11.7). Reconstruct rebuilds
        // the primary mailbox DB from the archive and intentionally materializes
        // NO atc_* tables here. Sidecar schema application is deferred until
        // promotion (its data is droppable telemetry and is never salvaged from
        // the archive). Reconstruct intentionally also leaves FTS-backed message
        // trigger follow-ups to the next live startup.

        // Rebuild all index b-trees to ensure consistency after bulk inserts.
        conn.execute_raw("REINDEX;")
            .map_err(|e| DbError::Sqlite(format!("reconstruct: REINDEX: {e}")))?;

        conn.execute_raw(&schema::schema_user_version_sql())
            .map_err(|e| DbError::Sqlite(format!("reconstruct: set user_version: {e}")))?;
        Ok(())
    })();

    if let Err(err) = rebuild_result {
        let _ = conn.execute_raw("ROLLBACK;");
        return Err(err);
    }
    conn.execute_raw("COMMIT;")
        .map_err(|e| DbError::Sqlite(format!("reconstruct: commit transaction: {e}")))?;
    drop(conn);
    crate::pool::wal_checkpoint_truncate_path(db_path)
        .map_err(|e| DbError::Sqlite(format!("reconstruct: checkpoint: {e}")))?;

    stats.finalize_duplicate_warnings();
    stats.finalize_cross_project_canonical_collision_warnings();
    tracing::info!(%stats, "database reconstruction from archive complete");
    Ok(stats)
}

/// Reconstruct the database from the Git archive and merge any additional
/// durable state from a salvaged `SQLite` database.
///
/// This is intended for doctor/recovery flows where the primary database file
/// was unhealthy, but a directly readable salvage database could still provide
/// additional rows that never made it into the Git archive, including DB-only
/// contact/product-bus metadata.
///
/// When a salvage path is supplied, probing and merging it are mandatory.
/// Returning an apparently successful archive-only result when the path is
/// missing, invalid, or unreadable — or when the merge fails — would silently
/// discard coordination state and allow callers to promote an incomplete
/// candidate. Callers that explicitly want archive-only recovery must pass
/// `None`.
pub fn reconstruct_from_archive_with_salvage(
    db_path: &Path,
    storage_root: &Path,
    salvage_db_path: Option<&Path>,
) -> DbResult<ReconstructStats> {
    if let Some(salvage_db_path) = salvage_db_path {
        probe_salvage_database_for_merge(salvage_db_path).map_err(|error| {
            DbError::Sqlite(format!(
                "reconstruct salvage source {} failed validation; refusing an archive-only candidate because DB-only coordination state could be lost: {error}",
                salvage_db_path.display()
            ))
        })?;
    }

    let mut stats =
        reconstruct_from_archive_impl(db_path, storage_root, salvage_db_path.is_some())?;
    if let Some(salvage_db_path) = salvage_db_path {
        merge_salvaged_database(db_path, salvage_db_path, &mut stats).map_err(|error| {
            DbError::Sqlite(format!(
                "reconstruct salvage merge from {} failed; refusing to promote the archive-only candidate because DB-only coordination state could be lost: {error}",
                salvage_db_path.display()
            ))
        })?;
    }
    Ok(stats)
}

fn probe_salvage_database_for_merge(path: &Path) -> DbResult<()> {
    crate::pool::validate_sqlite_target_path(path, "reconstruct salvage source")
        .map_err(|error| DbError::Sqlite(format!("reconstruct salvage: {error}")))?;
    if !is_real_file(path) {
        return Err(DbError::Sqlite(format!(
            "reconstruct salvage: candidate {} does not exist or is not a regular file",
            path.display()
        )));
    }
    let conn = open_read_only_salvage_db(path)?;
    conn.query_sync("SELECT name FROM sqlite_master LIMIT 1", &[])
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: cannot inspect candidate {}: {e}",
                path.display()
            ))
        })?;
    Ok(())
}

#[must_use]
#[cfg(test)]
fn is_reconstruct_benign_migration_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("already exists")
        || lower.contains("duplicate column name")
        || lower.contains("duplicate index name")
}

/// Walk `agents/{name}/profile.json` and insert agent rows.
fn discover_agents(
    conn: &DbConn,
    agents_dir: &Path,
    project_id: i64,
    agent_ids: &mut HashMap<(i64, String), i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return Ok(());
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Some(raw_agent_name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let Some(agent_name) = normalized_archive_agent_name(Some(&raw_agent_name)) else {
            stats.parse_errors += 1;
            stats.warnings.push(format!(
                "Archive agent directory {} has empty/invalid name; skipping profile",
                path.display()
            ));
            continue;
        };
        if agent_name != raw_agent_name {
            stats.warnings.push(format!(
                "Archive agent directory {} has non-canonical name {raw_agent_name:?}; normalizing to {agent_name:?}",
                path.display()
            ));
        }
        let profile_path = path.join("profile.json");
        if !is_real_file(&profile_path) {
            continue;
        }

        let profile_data = match read_archive_text_capped(&profile_path) {
            Ok(d) => d,
            Err(e) => {
                stats.parse_errors += 1;
                stats
                    .warnings
                    .push(format!("Cannot read {}: {e}", profile_path.display()));
                continue;
            }
        };

        let profile: serde_json::Value = match serde_json::from_str(&profile_data) {
            Ok(v) => v,
            Err(e) => {
                stats.parse_errors += 1;
                stats
                    .warnings
                    .push(format!("Cannot parse {}: {e}", profile_path.display()));
                continue;
            }
        };

        let profile_name = normalized_archive_agent_name(json_str(&profile, "name"));
        let agent_name = match profile_name {
            Some(profile_name) => {
                if profile_name != agent_name {
                    stats.warnings.push(format!(
                        "Archive agent profile {} has name {profile_name:?} that disagrees with directory name {raw_agent_name:?}; using profile name",
                        profile_path.display()
                    ));
                }
                profile_name
            }
            None => agent_name,
        };

        let profile_source = format!("archive agent profile {}", profile_path.display());
        let program = normalize_reconstructed_required_agent_field(
            json_str(&profile, "program"),
            &profile_source,
            "program",
            "unknown",
            stats,
        );
        let model = normalize_reconstructed_required_agent_field(
            json_str(&profile, "model"),
            &profile_source,
            "model",
            "unknown",
            stats,
        );
        let task_description = json_str(&profile, "task_description").unwrap_or("");
        let attachments_policy = normalize_reconstructed_attachments_policy(
            json_str(&profile, "attachments_policy"),
            &profile_source,
            stats,
        );
        let contact_policy = normalize_reconstructed_contact_policy(
            json_str(&profile, "contact_policy"),
            &profile_source,
            stats,
        );

        // Parse inception timestamp (try both field names for compatibility)
        let inception_ts = parse_ts_from_json(&profile, "inception_ts")
            .or_else(|| parse_ts_from_json(&profile, "registered_ts"));
        let last_active_ts = parse_ts_from_json(&profile, "last_active_ts")
            .unwrap_or_else(|| inception_ts.unwrap_or_else(crate::now_micros));
        let inception_ts = inception_ts.unwrap_or(last_active_ts);

        conn.execute_sync(
            "INSERT OR IGNORE INTO agents \
             (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(project_id),
                Value::Text(agent_name.clone()),
                Value::Text(program),
                Value::Text(model),
                Value::Text(task_description.to_string()),
                Value::BigInt(inception_ts),
                Value::BigInt(last_active_ts),
                Value::Text(attachments_policy),
                Value::Text(contact_policy),
            ],
        )
        .map_err(|e| DbError::Sqlite(format!("reconstruct: insert agent {agent_name}: {e}")))?;

        let aid = query_last_insert_or_existing_id_composite(
            conn,
            "agents",
            "project_id",
            project_id,
            "name",
            &agent_name,
        )?;
        agent_ids.insert((project_id, agent_name), aid);
        stats.agents += 1;
    }

    Ok(())
}

/// Walk `messages/{YYYY}/{MM}/*.md` and insert message + recipient rows.
///
/// Returns `Err` only for unrecoverable DB failures (connection dead, disk full).
/// Individual file parse errors are counted in `stats.parse_errors` and skipped.
fn discover_messages(
    conn: &DbConn,
    messages_dir: &Path,
    project_id: i64,
    project_slug: &str,
    agent_ids: &mut HashMap<(i64, String), i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    // Walk year directories
    let Ok(years) = std::fs::read_dir(messages_dir) else {
        return Ok(());
    };

    let mut message_files: Vec<PathBuf> = Vec::new();

    for year_entry in years.flatten() {
        let year_path = year_entry.path();
        let Ok(year_type) = year_entry.file_type() else {
            continue;
        };
        if !year_type.is_dir() || year_type.is_symlink() {
            continue;
        }
        // Walk month directories
        let Ok(months) = std::fs::read_dir(&year_path) else {
            continue;
        };
        for month_entry in months.flatten() {
            let month_path = month_entry.path();
            let Ok(month_type) = month_entry.file_type() else {
                continue;
            };
            if !month_type.is_dir() || month_type.is_symlink() {
                continue;
            }
            // Collect .md files
            let Ok(files) = std::fs::read_dir(&month_path) else {
                continue;
            };
            for file_entry in files.flatten() {
                let file_path = file_entry.path();
                let Ok(file_type) = file_entry.file_type() else {
                    continue;
                };
                if file_type.is_file()
                    && !file_type.is_symlink()
                    && file_path.extension().is_some_and(|e| e == "md")
                {
                    message_files.push(file_path);
                }
            }
        }
    }

    // Sort by filename (which starts with ISO timestamp) for chronological order
    message_files.sort();

    for file_path in &message_files {
        match parse_and_insert_message(conn, file_path, project_id, project_slug, agent_ids, stats)
        {
            Ok(()) => {}
            Err(e) => {
                // Distinguish parse errors (skip file) from DB errors (abort).
                // Probe the connection — if it's dead, propagate the error.
                if conn.execute_raw("SELECT 1").is_err() {
                    return Err(e);
                }
                stats.parse_errors += 1;
                stats.warnings.push(format!(
                    "Failed to reconstruct message from {}: {e}",
                    file_path.display()
                ));
            }
        }
    }
    Ok(())
}

/// Parse a single archive `.md` file and insert the message into the database.
#[allow(clippy::too_many_lines)]
fn parse_and_insert_message(
    conn: &DbConn,
    file_path: &Path,
    project_id: i64,
    _project_slug: &str,
    agent_ids: &mut HashMap<(i64, String), i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    let content = read_archive_text_capped(file_path)
        .map_err(|e| DbError::Sqlite(format!("read {}: {e}", file_path.display())))?;

    // Parse JSON frontmatter between ---json and ---
    let frontmatter = extract_json_frontmatter(&content).ok_or_else(|| {
        DbError::Sqlite(format!("no JSON frontmatter in {}", file_path.display()))
    })?;

    let msg: serde_json::Value = serde_json::from_str(frontmatter)
        .map_err(|e| DbError::Sqlite(format!("bad JSON in {}: {e}", file_path.display())))?;

    // Extract fields
    let sender_name = normalized_archive_agent_name(
        json_str(&msg, "from")
            .or_else(|| json_str(&msg, "sender"))
            .or_else(|| json_str(&msg, "from_agent")),
    )
    .unwrap_or_else(|| "unknown".to_string());

    let subject = json_str(&msg, "subject").unwrap_or("");
    let body_md = extract_body_after_frontmatter(&content).unwrap_or("");
    let raw_thread_id = json_str(&msg, "thread_id");
    let importance = json_str(&msg, "importance").unwrap_or("normal");
    let ack_required = msg
        .get("ack_required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let created_ts = parse_ts_from_json(&msg, "created_ts")
        .or_else(|| parse_ts_from_json(&msg, "created"))
        .unwrap_or_else(crate::now_micros);
    let attachments = normalize_archive_attachments_json(
        msg.get("attachments"),
        &file_path.display().to_string(),
        stats,
    );

    // Ensure sender agent exists
    let sender_id = ensure_agent_exists(conn, project_id, &sender_name, agent_ids)?;

    let (recipients_json, to_names, cc_names, bcc_names) =
        normalize_archive_recipients_json(&msg, &file_path.display().to_string(), stats);

    // Insert message, preserving canonical frontmatter ID when available.
    //
    // If the frontmatter contains a valid positive `id` field, use it as the
    // DB primary key so that archive filenames (which embed `__{id}.md`)
    // remain consistent with DB row IDs.
    // See: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/9
    let canonical_id = msg
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .filter(|&id| id > 0);

    // Canonical-id collision handling:
    //
    //   Same-project collision:  almost certainly a duplicate archive artifact
    //                            (two files for the same logical message).
    //                            Keep the first, skip the second.
    //
    //   Cross-project collision: two *different* messages in two separate
    //                            project archives happen to share the same
    //                            frontmatter `id` (e.g. because the archives
    //                            were originally produced by separate storage
    //                            roots). Both are real messages; skipping one
    //                            would drop legitimate data. Insert the
    //                            second under an auto-generated DB id and
    //                            record a warning so operators can audit.
    //
    // See: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/104
    let canonical_id = if let Some(cid) = canonical_id {
        if let Some(existing_project_id) = message_project_id(conn, cid)? {
            if existing_project_id == project_id {
                stats.record_duplicate_canonical_message(cid, file_path);
                return Ok(());
            }
            stats.record_cross_project_canonical_collision(
                cid,
                existing_project_id,
                project_id,
                file_path,
            );
            None
        } else {
            Some(cid)
        }
    } else {
        None
    };

    let thread_id = raw_thread_id.and_then(|raw| {
        let normalized = sanitize_reconstructed_thread_id(raw);
        if normalized.as_deref() != Some(raw) {
            stats.warnings.push(format!(
                "Sanitized invalid thread_id {:?} in {} during reconstruction",
                raw,
                file_path.display()
            ));
        }
        normalized
    });
    let thread_id_val = thread_id
        .as_deref()
        .map_or_else(|| Value::Null, |t| Value::Text(t.to_string()));

    let message_id = if let Some(cid) = canonical_id {
        conn.execute_sync(
            "INSERT OR REPLACE INTO messages \
             (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(cid),
                Value::BigInt(project_id),
                Value::BigInt(sender_id),
                thread_id_val,
                Value::Text(subject.to_string()),
                Value::Text(body_md.to_string()),
                Value::Text(importance.to_string()),
                Value::BigInt(i64::from(ack_required)),
                Value::BigInt(created_ts),
                Value::Text(recipients_json.clone()),
                Value::Text(attachments),
            ],
        )
        .map_err(|e| DbError::Sqlite(format!("insert message with id {cid}: {e}")))?;
        cid
    } else {
        conn.execute_sync(
            "INSERT INTO messages \
             (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(project_id),
                Value::BigInt(sender_id),
                thread_id_val,
                Value::Text(subject.to_string()),
                Value::Text(body_md.to_string()),
                Value::Text(importance.to_string()),
                Value::BigInt(i64::from(ack_required)),
                Value::BigInt(created_ts),
                Value::Text(recipients_json.clone()),
                Value::Text(attachments),
            ],
        )
        .map_err(|e| DbError::Sqlite(format!("insert message: {e}")))?;

        // Retrieve the inserted row ID via last_insert_rowid() for reliability.
        query_last_insert_rowid(conn)?
    };

    stats.messages += 1;

    // Insert recipients
    for name in &to_names {
        let aid = ensure_agent_exists(conn, project_id, name, agent_ids)?;
        insert_recipient(conn, message_id, aid, "to")?;
        stats.recipients += 1;
    }
    for name in &cc_names {
        let aid = ensure_agent_exists(conn, project_id, name, agent_ids)?;
        insert_recipient(conn, message_id, aid, "cc")?;
        stats.recipients += 1;
    }
    for name in &bcc_names {
        let aid = ensure_agent_exists(conn, project_id, name, agent_ids)?;
        insert_recipient(conn, message_id, aid, "bcc")?;
        stats.recipients += 1;
    }

    Ok(())
}

/// Ensure an agent row exists, creating a placeholder if needed.
fn ensure_agent_exists(
    conn: &DbConn,
    project_id: i64,
    name: &str,
    agent_ids: &mut HashMap<(i64, String), i64>,
) -> DbResult<i64> {
    let key = (project_id, name.to_string());
    if let Some(&id) = agent_ids.get(&key) {
        return Ok(id);
    }

    let now = crate::now_micros();
    conn.execute_sync(
        "INSERT OR IGNORE INTO agents \
         (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
         VALUES (?, ?, 'unknown', 'unknown', '', ?, ?, 'auto', 'auto')",
        &[
            Value::BigInt(project_id),
            Value::Text(name.to_string()),
            Value::BigInt(now),
            Value::BigInt(now),
        ],
    )
    .map_err(|e| DbError::Sqlite(format!("ensure agent {name}: {e}")))?;

    let aid = query_last_insert_or_existing_id_composite(
        conn,
        "agents",
        "project_id",
        project_id,
        "name",
        name,
    )?;
    agent_ids.insert(key, aid);
    Ok(aid)
}

fn insert_recipient(conn: &DbConn, message_id: i64, agent_id: i64, kind: &str) -> DbResult<()> {
    conn.execute_sync(
        "INSERT OR IGNORE INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)",
        &[
            Value::BigInt(message_id),
            Value::BigInt(agent_id),
            Value::Text(kind.to_string()),
        ],
    )
    .map(|_| ())
    .map_err(|e| DbError::Sqlite(format!("insert recipient: {e}")))
}

fn encode_recipients_json(
    to_names: &[String],
    cc_names: &[String],
    bcc_names: &[String],
) -> String {
    serde_json::json!({
        "to": to_names,
        "cc": cc_names,
        "bcc": bcc_names,
    })
    .to_string()
}

fn normalize_salvaged_recipient_kind(
    raw_kind: Option<&str>,
    message_id: i64,
    stats: &mut ReconstructStats,
) -> String {
    let Some(trimmed) = raw_kind.map(str::trim).filter(|kind| !kind.is_empty()) else {
        return "to".to_string();
    };
    match trimmed.to_ascii_lowercase().as_str() {
        "to" => "to".to_string(),
        "cc" => "cc".to_string(),
        "bcc" => "bcc".to_string(),
        _ => {
            stats.warnings.push(format!(
                "Salvage recipient for message {message_id} had invalid kind {trimmed:?}; defaulting to \"to\""
            ));
            "to".to_string()
        }
    }
}

fn malformed_attachments_json() -> String {
    serde_json::json!([{
        "name": MALFORMED_ATTACHMENTS_SENTINEL,
        "media_type": serde_json::Value::Null,
        "path": serde_json::Value::Null,
        "bytes": serde_json::Value::Null,
    }])
    .to_string()
}

fn normalize_archive_attachments_json(
    attachments: Option<&serde_json::Value>,
    message_label: &str,
    stats: &mut ReconstructStats,
) -> String {
    match attachments {
        None => "[]".to_string(),
        Some(serde_json::Value::Array(values)) => {
            serde_json::Value::Array(values.clone()).to_string()
        }
        Some(_) => {
            stats.warnings.push(format!(
                "Archive message {message_label} has non-array attachments payload; preserving malformed attachment metadata sentinel"
            ));
            malformed_attachments_json()
        }
    }
}

fn normalize_archive_recipients_json(
    msg: &serde_json::Value,
    message_label: &str,
    stats: &mut ReconstructStats,
) -> (String, Vec<String>, Vec<String>, Vec<String>) {
    if !reconstructed_recipients_payload_is_valid(msg) {
        stats.warnings.push(format!(
            "Archive message {message_label} has non-canonical recipient payload; preserving malformed recipient metadata sentinel"
        ));
        return (
            encode_recipients_json(&[MALFORMED_RECIPIENTS_SENTINEL.to_string()], &[], &[]),
            vec![MALFORMED_RECIPIENTS_SENTINEL.to_string()],
            Vec::new(),
            Vec::new(),
        );
    }

    let to_names = json_str_array(msg, "to");
    let cc_names = json_str_array(msg, "cc");
    let bcc_names = json_str_array(msg, "bcc");
    (
        encode_recipients_json(&to_names, &cc_names, &bcc_names),
        to_names,
        cc_names,
        bcc_names,
    )
}

fn parse_salvaged_attachments_json(
    attachments_json: Option<String>,
    message_id: i64,
    stats: &mut ReconstructStats,
) -> String {
    let Some(attachments_json) = attachments_json.filter(|json| !json.trim().is_empty()) else {
        return "[]".to_string();
    };

    match serde_json::from_str::<serde_json::Value>(&attachments_json) {
        Ok(serde_json::Value::Array(values)) => serde_json::Value::Array(values).to_string(),
        Ok(_) => {
            stats.warnings.push(format!(
                "Salvage message {message_id} has non-array attachments payload; preserving malformed attachment metadata sentinel"
            ));
            malformed_attachments_json()
        }
        Err(err) => {
            stats.warnings.push(format!(
                "Salvage message {message_id} has invalid attachments payload; preserving malformed attachment metadata sentinel: {err}"
            ));
            malformed_attachments_json()
        }
    }
}

fn parse_salvaged_recipients_json(
    recipients_json: Option<String>,
    message_id: i64,
    stats: &mut ReconstructStats,
) -> (String, Vec<String>, Vec<String>, Vec<String>) {
    let empty = (
        encode_recipients_json(&[], &[], &[]),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let Some(recipients_json) = recipients_json.filter(|json| !json.trim().is_empty()) else {
        return empty;
    };

    let malformed = || {
        (
            encode_recipients_json(&[MALFORMED_RECIPIENTS_SENTINEL.to_string()], &[], &[]),
            vec![MALFORMED_RECIPIENTS_SENTINEL.to_string()],
            Vec::new(),
            Vec::new(),
        )
    };

    let parsed: serde_json::Value = match serde_json::from_str(&recipients_json) {
        Ok(parsed) => parsed,
        Err(err) => {
            stats.warnings.push(format!(
                "Salvage message {message_id} has invalid recipients_json; preserving malformed recipient metadata sentinel: {err}"
            ));
            return malformed();
        }
    };
    if !reconstructed_recipients_payload_is_valid(&parsed) {
        stats.warnings.push(format!(
            "Salvage message {message_id} has non-canonical recipients_json; preserving malformed recipient metadata sentinel"
        ));
        return malformed();
    }

    let to_names = json_str_array(&parsed, "to");
    let cc_names = json_str_array(&parsed, "cc");
    let bcc_names = json_str_array(&parsed, "bcc");
    (
        encode_recipients_json(&to_names, &cc_names, &bcc_names),
        to_names,
        cc_names,
        bcc_names,
    )
}

fn sync_reconstructed_message_recipients_json(conn: &DbConn, message_id: i64) -> DbResult<()> {
    let rows = conn
        .query_sync(
            "SELECT CASE WHEN a.id IS NULL THEN '[unknown-agent-' || mr.agent_id || ']' ELSE TRIM(a.name) END AS name, \
                    mr.kind AS kind \
             FROM message_recipients mr \
             LEFT JOIN agents a ON a.id = mr.agent_id \
             WHERE mr.message_id = ? \
             ORDER BY CASE mr.kind WHEN 'to' THEN 0 WHEN 'cc' THEN 1 WHEN 'bcc' THEN 2 ELSE 3 END, \
                     CASE WHEN a.id IS NULL THEN '[unknown-agent-' || mr.agent_id || ']' ELSE TRIM(a.name) END COLLATE NOCASE",
            &[Value::BigInt(message_id)],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: query recipients_json rows for message {message_id}: {e}"
            ))
        })?;

    let mut to_names = Vec::new();
    let mut cc_names = Vec::new();
    let mut bcc_names = Vec::new();

    for row in rows {
        let raw_name = row.get_named::<String>("name").map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: decode recipient name for message {message_id}: {e}"
            ))
        })?;
        let Some(name) = normalized_archive_agent_name(Some(raw_name.as_str())) else {
            continue;
        };
        let kind = row.get_named::<String>("kind").map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: decode recipient kind for message {message_id}: {e}"
            ))
        })?;
        match kind.as_str() {
            "cc" => cc_names.push(name),
            "bcc" => bcc_names.push(name),
            _ => to_names.push(name),
        }
    }

    conn.execute_sync(
        "UPDATE messages SET recipients_json = ? WHERE id = ?",
        &[
            Value::Text(encode_recipients_json(&to_names, &cc_names, &bcc_names)),
            Value::BigInt(message_id),
        ],
    )
    .map(|_| ())
    .map_err(|e| {
        DbError::Sqlite(format!(
            "reconstruct salvage: update recipients_json for message {message_id}: {e}"
        ))
    })
}

struct ArchivedFileReservation {
    reservation_id: Option<i64>,
    agent_name: String,
    path_pattern: String,
    exclusive: bool,
    reason: String,
    created_ts: i64,
    expires_ts: i64,
    released_ts: Option<i64>,
}

fn reservation_artifact_paths(reservations_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(reservations_dir) else {
        return Vec::new();
    };

    let mut reservation_files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file()
            && !file_type.is_symlink()
            && path.extension().is_some_and(|ext| ext == "json")
        {
            reservation_files.push(path);
        }
    }
    reservation_files.sort();
    reservation_files
}

fn parse_archived_file_reservation(
    file_path: &Path,
    stats: &mut ReconstructStats,
) -> Option<ArchivedFileReservation> {
    let reservation_data = match read_archive_text_capped(file_path) {
        Ok(data) => data,
        Err(e) => {
            stats.parse_errors += 1;
            stats.warnings.push(format!(
                "Cannot read reservation artifact {}: {e}",
                file_path.display()
            ));
            return None;
        }
    };

    let reservation: serde_json::Value = match serde_json::from_str(&reservation_data) {
        Ok(value) => value,
        Err(e) => {
            stats.parse_errors += 1;
            stats.warnings.push(format!(
                "Cannot parse reservation artifact {}: {e}",
                file_path.display()
            ));
            return None;
        }
    };

    let Some(path_pattern) = json_str(&reservation, "path_pattern")
        .or_else(|| json_str(&reservation, "path"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
    else {
        stats.parse_errors += 1;
        stats.warnings.push(format!(
            "Reservation artifact {} is missing path_pattern/path",
            file_path.display()
        ));
        return None;
    };

    let agent_name = normalized_archive_agent_name(json_str(&reservation, "agent"))
        .unwrap_or_else(|| "unknown".to_string());
    let exclusive = reservation
        .get("exclusive")
        .and_then(|value| value.as_bool().or_else(|| value.as_i64().map(|n| n != 0)))
        .unwrap_or(true);
    let reason = json_str(&reservation, "reason").unwrap_or("").to_string();
    let created_ts =
        parse_ts_from_json(&reservation, "created_ts").unwrap_or_else(crate::now_micros);
    let expires_ts = parse_ts_from_json(&reservation, "expires_ts").unwrap_or(created_ts);
    let released_ts = parse_ts_from_json(&reservation, "released_ts");
    let reservation_id = reservation
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0);

    Some(ArchivedFileReservation {
        reservation_id,
        agent_name,
        path_pattern,
        exclusive,
        reason,
        created_ts,
        expires_ts,
        released_ts,
    })
}

fn insert_archived_file_reservation(
    conn: &DbConn,
    project_id: i64,
    reservation: &ArchivedFileReservation,
    file_path: &Path,
    agent_ids: &mut HashMap<(i64, String), i64>,
) -> DbResult<()> {
    let agent_id = ensure_agent_exists(conn, project_id, &reservation.agent_name, agent_ids)?;

    if let Some(id) = reservation.reservation_id {
        conn.execute_sync(
            "INSERT OR REPLACE INTO file_reservations \
             (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(id),
                Value::BigInt(project_id),
                Value::BigInt(agent_id),
                Value::Text(reservation.path_pattern.clone()),
                Value::BigInt(i64::from(reservation.exclusive)),
                Value::Text(reservation.reason.clone()),
                Value::BigInt(reservation.created_ts),
                Value::BigInt(reservation.expires_ts),
                reservation.released_ts.map_or(Value::Null, Value::BigInt),
            ],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct: insert file reservation {}: {e}",
                file_path.display()
            ))
        })?;
    } else {
        conn.execute_sync(
            "INSERT INTO file_reservations \
             (project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(project_id),
                Value::BigInt(agent_id),
                Value::Text(reservation.path_pattern.clone()),
                Value::BigInt(i64::from(reservation.exclusive)),
                Value::Text(reservation.reason.clone()),
                Value::BigInt(reservation.created_ts),
                Value::BigInt(reservation.expires_ts),
                reservation.released_ts.map_or(Value::Null, Value::BigInt),
            ],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct: insert file reservation {}: {e}",
                file_path.display()
            ))
        })?;
    }

    Ok(())
}

fn discover_file_reservations(
    conn: &DbConn,
    reservations_dir: &Path,
    project_id: i64,
    agent_ids: &mut HashMap<(i64, String), i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    for file_path in reservation_artifact_paths(reservations_dir) {
        let Some(reservation) = parse_archived_file_reservation(&file_path, stats) else {
            continue;
        };
        insert_archived_file_reservation(conn, project_id, &reservation, &file_path, agent_ids)?;
    }

    Ok(())
}

fn sanitize_reconstructed_thread_id(raw: &str) -> Option<String> {
    let sanitized: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
        .take(128)
        .collect();
    if sanitized.is_empty()
        || !sanitized
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        None
    } else {
        Some(sanitized)
    }
}

/// Return the `project_id` of a message row with the given canonical id, or
/// `None` if no such row exists. Used during reconstruction to distinguish
/// same-project duplicates from cross-project canonical-id collisions.
fn message_project_id(conn: &DbConn, message_id: i64) -> DbResult<Option<i64>> {
    let rows = conn
        .query_sync(
            "SELECT project_id FROM messages WHERE id = ? LIMIT 1",
            &[Value::BigInt(message_id)],
        )
        .map_err(|e| DbError::Sqlite(format!("check message {message_id} project: {e}")))?;
    if let Some(row) = rows.first() {
        let pid = row.get_named::<i64>("project_id").map_err(|e| {
            DbError::Sqlite(format!("decode project_id for message {message_id}: {e}"))
        })?;
        Ok(Some(pid))
    } else {
        Ok(None)
    }
}

fn agent_project_id(conn: &DbConn, agent_id: i64) -> DbResult<Option<i64>> {
    let rows = conn
        .query_sync(
            "SELECT project_id FROM agents WHERE id = ? LIMIT 1",
            &[Value::BigInt(agent_id)],
        )
        .map_err(|e| DbError::Sqlite(format!("check agent {agent_id} project: {e}")))?;
    if let Some(row) = rows.first() {
        let project_id = row
            .get_named::<i64>("project_id")
            .map_err(|e| DbError::Sqlite(format!("decode project_id for agent {agent_id}: {e}")))?;
        Ok(Some(project_id))
    } else {
        Ok(None)
    }
}

fn table_exists(conn: &DbConn, table: &str) -> DbResult<bool> {
    let rows = conn
        .query_sync(
            "SELECT 1 AS exists_flag FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
            &[Value::Text(table.to_string())],
        )
        .map_err(|e| DbError::Sqlite(format!("check table {table} existence: {e}")))?;
    Ok(!rows.is_empty())
}

fn table_columns(conn: &DbConn, table: &str) -> DbResult<HashSet<String>> {
    let rows = conn
        .query_sync(&format!("PRAGMA table_info({table})"), &[])
        .map_err(|e| DbError::Sqlite(format!("inspect columns for {table}: {e}")))?;
    let mut columns = HashSet::new();
    for row in &rows {
        if let Ok(name) = row.get_named::<String>("name") {
            columns.insert(name);
        }
    }
    Ok(columns)
}

fn build_salvage_select(
    table: &str,
    columns: &HashSet<String>,
    required: &[&str],
    optional: &[&str],
    stats: &mut ReconstructStats,
    salvage_db_path: &Path,
) -> Option<String> {
    let missing_required: Vec<&str> = required
        .iter()
        .copied()
        .filter(|column| !columns.contains(*column))
        .collect();
    if !missing_required.is_empty() {
        stats.warnings.push(format!(
            "Salvage database {} table {table} missing required column(s): {}",
            salvage_db_path.display(),
            missing_required.join(", ")
        ));
        return None;
    }

    let mut selected = required
        .iter()
        .map(|column| (*column).to_string())
        .collect::<Vec<_>>();
    selected.extend(
        optional
            .iter()
            .copied()
            .filter(|column| columns.contains(*column))
            .map(str::to_string),
    );
    Some(selected.join(", "))
}

fn build_salvage_agent_links_select(
    columns: &HashSet<String>,
    stats: &mut ReconstructStats,
    salvage_db_path: &Path,
) -> Option<String> {
    const CURRENT_REQUIRED: [&str; 4] =
        ["a_project_id", "a_agent_id", "b_project_id", "b_agent_id"];
    const LEGACY_REQUIRED: [&str; 3] = ["project_id", "from_agent_id", "to_agent_id"];
    const OPTIONAL: [&str; 5] = ["status", "reason", "created_ts", "updated_ts", "expires_ts"];

    if CURRENT_REQUIRED
        .iter()
        .all(|column| columns.contains(*column))
    {
        return build_salvage_select(
            "agent_links",
            columns,
            &CURRENT_REQUIRED,
            &OPTIONAL,
            stats,
            salvage_db_path,
        );
    }

    if LEGACY_REQUIRED
        .iter()
        .all(|column| columns.contains(*column))
    {
        let mut selected = vec![
            "project_id AS a_project_id".to_string(),
            "from_agent_id AS a_agent_id".to_string(),
            "project_id AS b_project_id".to_string(),
            "to_agent_id AS b_agent_id".to_string(),
        ];
        selected.extend(
            OPTIONAL
                .iter()
                .copied()
                .filter(|column| columns.contains(*column))
                .map(str::to_string),
        );
        return Some(selected.join(", "));
    }

    let missing_current = CURRENT_REQUIRED
        .iter()
        .copied()
        .filter(|column| !columns.contains(*column))
        .collect::<Vec<_>>()
        .join(", ");
    let missing_legacy = LEGACY_REQUIRED
        .iter()
        .copied()
        .filter(|column| !columns.contains(*column))
        .collect::<Vec<_>>()
        .join(", ");
    stats.warnings.push(format!(
        "Salvage database {} table agent_links missing required columns for both current schema ({missing_current}) and legacy schema ({missing_legacy})",
        salvage_db_path.display()
    ));
    None
}

fn merge_salvaged_created_at(current_created_at: i64, salvaged_created_at: i64) -> i64 {
    if salvaged_created_at <= 0 {
        current_created_at
    } else if current_created_at <= 0 {
        salvaged_created_at
    } else {
        current_created_at.min(salvaged_created_at)
    }
}

fn merge_salvaged_inception_ts(current_inception_ts: i64, salvaged_inception_ts: i64) -> i64 {
    if salvaged_inception_ts <= 0 {
        current_inception_ts
    } else if current_inception_ts <= 0 {
        salvaged_inception_ts
    } else {
        current_inception_ts.min(salvaged_inception_ts)
    }
}

fn merge_salvaged_last_active_ts(current_last_active_ts: i64, salvaged_last_active_ts: i64) -> i64 {
    if salvaged_last_active_ts <= 0 {
        current_last_active_ts
    } else if current_last_active_ts <= 0 {
        salvaged_last_active_ts
    } else {
        current_last_active_ts.max(salvaged_last_active_ts)
    }
}

fn should_replace_placeholder_text(current: &str, salvaged: &str, placeholder: &str) -> bool {
    let current = current.trim();
    let salvaged = salvaged.trim();
    !salvaged.is_empty()
        && salvaged != placeholder
        && (current.is_empty() || current == placeholder)
}

fn should_replace_default_policy(current: &str, salvaged: &str) -> bool {
    let current = current.trim();
    let salvaged = salvaged.trim();
    !salvaged.is_empty() && salvaged != "auto" && (current.is_empty() || current == "auto")
}

fn synthetic_project_placeholder_human_key(slug: &str) -> String {
    format!("/{slug}")
}

fn normalized_project_match_token(value: &str) -> Option<String> {
    let normalized = value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect::<String>();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn project_basename_token_for_human_key(human_key: &str) -> Option<String> {
    let trimmed = human_key.trim();
    if trimmed.is_empty() {
        return None;
    }
    let basename = Path::new(trimmed).file_name()?.to_str()?;
    normalized_project_match_token(basename)
}

fn is_synthetic_project_placeholder(slug: &str, human_key: &str) -> bool {
    let trimmed = human_key.trim();
    trimmed.is_empty() || trimmed == synthetic_project_placeholder_human_key(slug)
}

fn validate_salvage_project_identity_match(
    target_slug: &str,
    target_human_key: &str,
    salvaged_slug: &str,
    salvaged_human_key: &str,
) -> DbResult<()> {
    let target_is_placeholder = is_synthetic_project_placeholder(target_slug, target_human_key);
    let salvage_is_placeholder =
        is_synthetic_project_placeholder(salvaged_slug, salvaged_human_key);
    if !target_is_placeholder
        && !salvage_is_placeholder
        && target_human_key.trim() != salvaged_human_key.trim()
    {
        return Err(DbError::Sqlite(format!(
            "reconstruct salvage: project slug {salvaged_slug:?} resolves to conflicting canonical human keys {:?} and {:?}; refusing to merge distinct project identities",
            target_human_key.trim(),
            salvaged_human_key.trim()
        )));
    }
    Ok(())
}

fn enrich_existing_project_from_salvage(
    conn: &DbConn,
    project_id: i64,
    slug: &str,
    salvaged_slug: &str,
    salvaged_human_key: &str,
    salvaged_created_at: i64,
) -> DbResult<()> {
    let existing_rows = conn
        .query_sync(
            "SELECT slug, human_key, created_at FROM projects WHERE id = ? LIMIT 1",
            &[Value::BigInt(project_id)],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: query project state for slug {slug}: {e}"
            ))
        })?;
    let Some(existing_row) = existing_rows.first() else {
        return Ok(());
    };

    let current_slug = existing_row
        .get_named::<String>("slug")
        .unwrap_or_else(|_| slug.to_string());
    let current_human_key = existing_row
        .get_named::<String>("human_key")
        .unwrap_or_else(|_| synthetic_project_placeholder_human_key(&current_slug));
    let current_created_at = existing_row
        .get_named::<i64>("created_at")
        .unwrap_or_default();
    validate_salvage_project_identity_match(
        &current_slug,
        &current_human_key,
        salvaged_slug,
        salvaged_human_key,
    )?;
    let fallback_human_key = synthetic_project_placeholder_human_key(&current_slug);
    let current_is_placeholder =
        current_human_key.trim().is_empty() || current_human_key == fallback_human_key;
    let next_slug = if current_is_placeholder {
        let candidate = salvaged_slug.trim();
        if candidate.is_empty() {
            current_slug.clone()
        } else {
            candidate.to_string()
        }
    } else {
        current_slug.clone()
    };
    let next_human_key = if current_is_placeholder {
        let candidate = salvaged_human_key.trim();
        if Path::new(candidate).is_absolute() {
            candidate.to_string()
        } else {
            current_human_key.clone()
        }
    } else {
        current_human_key.clone()
    };
    let next_created_at = merge_salvaged_created_at(current_created_at, salvaged_created_at);

    if next_slug != current_slug
        || next_human_key != current_human_key
        || next_created_at != current_created_at
    {
        conn.execute_sync(
            "UPDATE projects SET slug = ?, human_key = ?, created_at = ? WHERE id = ?",
            &[
                Value::Text(next_slug),
                Value::Text(next_human_key),
                Value::BigInt(next_created_at),
                Value::BigInt(project_id),
            ],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: enrich project metadata for slug {slug}: {e}"
            ))
        })?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn enrich_existing_agent_from_salvage(
    conn: &DbConn,
    agent_id: i64,
    name: &str,
    salvaged_program: &str,
    salvaged_model: &str,
    salvaged_task_description: &str,
    salvaged_inception_ts: i64,
    salvaged_last_active_ts: i64,
    salvaged_attachments_policy: &str,
    salvaged_contact_policy: &str,
    salvaged_reaper_exempt: Option<bool>,
    salvaged_registration_token: Option<&str>,
    salvage_has_registration_token: bool,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    let existing_rows = conn
        .query_sync(
            "SELECT program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, registration_token \
             FROM agents WHERE id = ? LIMIT 1",
            &[Value::BigInt(agent_id)],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: query agent state for {name}: {e}"
            ))
        })?;
    let Some(existing_row) = existing_rows.first() else {
        return Ok(());
    };

    let current_program_raw = existing_row.get_named::<String>("program").ok();
    let current_model_raw = existing_row.get_named::<String>("model").ok();
    let current_task_description = existing_row
        .get_named::<String>("task_description")
        .unwrap_or_default();
    let current_inception_ts = existing_row
        .get_named::<i64>("inception_ts")
        .unwrap_or_default();
    let current_last_active_ts = existing_row
        .get_named::<i64>("last_active_ts")
        .unwrap_or_default();
    let current_attachments_policy_raw =
        existing_row.get_named::<String>("attachments_policy").ok();
    let current_contact_policy_raw = existing_row.get_named::<String>("contact_policy").ok();
    let current_reaper_exempt = existing_row
        .get_named::<i64>("reaper_exempt")
        .is_ok_and(|value| value != 0);
    let current_registration_token = existing_row
        .get_named::<Option<String>>("registration_token")
        .unwrap_or_default();
    if salvage_has_registration_token
        && let (Some(current), Some(salvaged)) = (
            current_registration_token.as_deref(),
            salvaged_registration_token,
        )
        && current != salvaged
    {
        return Err(DbError::Sqlite(format!(
            "reconstruct salvage: agent {name} has conflicting non-null registration tokens; refusing to bind credentials across ambiguous identities"
        )));
    }
    let existing_source = format!("existing agent row {agent_id} ({name})");
    let current_program = normalize_reconstructed_required_agent_field(
        current_program_raw.as_deref(),
        &existing_source,
        "program",
        "unknown",
        stats,
    );
    let current_model = normalize_reconstructed_required_agent_field(
        current_model_raw.as_deref(),
        &existing_source,
        "model",
        "unknown",
        stats,
    );
    let current_attachments_policy = normalize_reconstructed_attachments_policy(
        current_attachments_policy_raw.as_deref(),
        &existing_source,
        stats,
    );
    let current_contact_policy = normalize_reconstructed_contact_policy(
        current_contact_policy_raw.as_deref(),
        &existing_source,
        stats,
    );
    let is_placeholder_agent = current_program.trim() == "unknown"
        && current_model.trim() == "unknown"
        && current_task_description.trim().is_empty()
        && current_attachments_policy.trim() == "auto"
        && current_contact_policy.trim() == "auto";

    let next_program =
        if should_replace_placeholder_text(&current_program, salvaged_program, "unknown") {
            salvaged_program.trim().to_string()
        } else {
            current_program.clone()
        };
    let next_model = if should_replace_placeholder_text(&current_model, salvaged_model, "unknown") {
        salvaged_model.trim().to_string()
    } else {
        current_model.clone()
    };
    let next_task_description = if should_replace_placeholder_text(
        &current_task_description,
        salvaged_task_description,
        "",
    ) {
        salvaged_task_description.trim().to_string()
    } else {
        current_task_description.clone()
    };
    let next_inception_ts =
        merge_salvaged_inception_ts(current_inception_ts, salvaged_inception_ts);
    let next_last_active_ts = if is_placeholder_agent && salvaged_last_active_ts > 0 {
        salvaged_last_active_ts
    } else {
        merge_salvaged_last_active_ts(current_last_active_ts, salvaged_last_active_ts)
    };
    let next_attachments_policy = if should_replace_default_policy(
        &current_attachments_policy,
        salvaged_attachments_policy,
    ) {
        salvaged_attachments_policy.trim().to_string()
    } else {
        current_attachments_policy.clone()
    };
    let next_contact_policy =
        if should_replace_default_policy(&current_contact_policy, salvaged_contact_policy) {
            salvaged_contact_policy.trim().to_string()
        } else {
            current_contact_policy.clone()
        };
    let next_reaper_exempt = salvaged_reaper_exempt.unwrap_or(current_reaper_exempt);
    let next_registration_token = if salvage_has_registration_token {
        salvaged_registration_token.map(str::to_string)
    } else {
        current_registration_token.clone()
    };

    if next_program != current_program
        || next_model != current_model
        || next_task_description != current_task_description
        || next_inception_ts != current_inception_ts
        || next_last_active_ts != current_last_active_ts
        || next_attachments_policy != current_attachments_policy
        || next_contact_policy != current_contact_policy
        || next_reaper_exempt != current_reaper_exempt
        || next_registration_token != current_registration_token
    {
        conn.execute_sync(
            "UPDATE agents SET \
                 program = ?, \
                 model = ?, \
                 task_description = ?, \
                 inception_ts = ?, \
                 last_active_ts = ?, \
                 attachments_policy = ?, \
                 contact_policy = ?, \
                 reaper_exempt = ?, \
                 registration_token = ? \
             WHERE id = ?",
            &[
                Value::Text(next_program),
                Value::Text(next_model),
                Value::Text(next_task_description),
                Value::BigInt(next_inception_ts),
                Value::BigInt(next_last_active_ts),
                Value::Text(next_attachments_policy),
                Value::Text(next_contact_policy),
                Value::BigInt(i64::from(next_reaper_exempt)),
                next_registration_token.map_or(Value::Null, Value::Text),
                Value::BigInt(agent_id),
            ],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: enrich agent metadata for {name}: {e}"
            ))
        })?;
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
fn merge_salvaged_database(
    target_db_path: &Path,
    salvage_db_path: &Path,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    let target_conn =
        DbConn::open_file(target_db_path.to_string_lossy().as_ref()).map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: cannot open target {}: {e}",
                target_db_path.display()
            ))
        })?;
    let salvage_conn = open_read_only_salvage_db(salvage_db_path)?;

    let has_projects = table_exists(&salvage_conn, "projects")?;
    let has_agents = table_exists(&salvage_conn, "agents")?;
    let has_messages = table_exists(&salvage_conn, "messages")?;
    let has_recipients = table_exists(&salvage_conn, "message_recipients")?;
    let has_agent_links = table_exists(&salvage_conn, "agent_links")?;
    let has_file_reservations = table_exists(&salvage_conn, "file_reservations")?;
    let has_file_reservation_releases = table_exists(&salvage_conn, "file_reservation_releases")?;
    let has_products = table_exists(&salvage_conn, "products")?;
    let has_product_project_links = table_exists(&salvage_conn, "product_project_links")?;
    let has_proof_gate_consumed_nonces = table_exists(&salvage_conn, "proof_gate_consumed_nonces")?;

    if !(has_projects
        || has_agents
        || has_messages
        || has_recipients
        || has_agent_links
        || has_file_reservations
        || has_file_reservation_releases
        || has_products
        || has_product_project_links
        || has_proof_gate_consumed_nonces)
    {
        stats.warnings.push(format!(
            "Salvage database {} contained none of the expected mail/product tables",
            salvage_db_path.display()
        ));
        return Ok(());
    }

    target_conn
        .execute_raw("BEGIN IMMEDIATE;")
        .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: begin transaction: {e}")))?;

    let pre_merge_stats = stats.clone();
    let merge_result: DbResult<()> = (|| {
        let mut project_id_map: HashMap<i64, i64> = HashMap::new();
        let mut agent_id_map: HashMap<i64, i64> = HashMap::new();
        let mut message_id_map: HashMap<i64, i64> = HashMap::new();
        let mut reservation_id_map: HashMap<i64, i64> = HashMap::new();
        let mut product_id_map: HashMap<i64, i64> = HashMap::new();
        if has_projects {
            let project_columns = table_columns(&salvage_conn, "projects")?;
            let project_select = build_salvage_select(
                "projects",
                &project_columns,
                &["id", "slug"],
                &["human_key", "created_at"],
                stats,
                salvage_db_path,
            )
            .ok_or_else(|| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: projects schema is incomplete in {}",
                    salvage_db_path.display()
                ))
            })?;
            let project_rows = salvage_conn
                .query_sync(
                    &format!("SELECT {project_select} FROM projects ORDER BY id"),
                    &[],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: query projects: {e}"))
                })?;

            for row in &project_rows {
                let source_project_id = row.get_named::<i64>("id").map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: decode project id: {e}"))
                })?;
                if source_project_id <= 0 {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: project has non-positive id {source_project_id}"
                    )));
                }
                let slug = row.get_named::<String>("slug").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode slug for project {source_project_id}: {e}"
                    ))
                })?;
                let slug = slug.trim().to_string();
                if slug.is_empty() {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: project {source_project_id} has an empty stable slug"
                    )));
                }

                let human_key = row
                    .get_named::<String>("human_key")
                    .unwrap_or_else(|_| synthetic_project_placeholder_human_key(&slug));
                let created_at = row
                    .get_named::<i64>("created_at")
                    .unwrap_or_else(|_| crate::now_micros());

                if let Ok(target_project_id) =
                    query_last_insert_or_existing_id(&target_conn, "projects", "slug", &slug)
                {
                    enrich_existing_project_from_salvage(
                        &target_conn,
                        target_project_id,
                        &slug,
                        &slug,
                        &human_key,
                        created_at,
                    )?;
                    project_id_map.insert(source_project_id, target_project_id);
                    continue;
                }
                // A basename-only match (for example `/shared` versus
                // `/srv/team-a/shared`) is not a stable project identity. Two
                // unrelated repositories routinely share a basename, and
                // merging them here would remap every salvaged child row to
                // the wrong project. Only exact slug or exact canonical
                // human-key matches may reuse an existing target row.
                if let Ok(target_project_id) = query_last_insert_or_existing_id(
                    &target_conn,
                    "projects",
                    "human_key",
                    &human_key,
                ) {
                    enrich_existing_project_from_salvage(
                        &target_conn,
                        target_project_id,
                        &slug,
                        &slug,
                        &human_key,
                        created_at,
                    )?;
                    project_id_map.insert(source_project_id, target_project_id);
                    continue;
                }
                target_conn
                    .execute_sync(
                        "INSERT OR IGNORE INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
                        &[
                            Value::Text(slug.clone()),
                            Value::Text(human_key),
                            Value::BigInt(created_at),
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!("reconstruct salvage: insert project {slug}: {e}"))
                    })?;
                let target_project_id =
                    query_last_insert_or_existing_id(&target_conn, "projects", "slug", &slug)?;
                project_id_map.insert(source_project_id, target_project_id);
                stats.salvaged_projects += 1;
            }

            #[cfg(test)]
            if FAIL_SALVAGE_MERGE_AFTER_PROJECTS.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(DbError::Sqlite(
                    "reconstruct salvage: forced failure after projects".to_string(),
                ));
            }
        }

        if has_agents {
            let agent_columns = table_columns(&salvage_conn, "agents")?;
            let agent_select = build_salvage_select(
                "agents",
                &agent_columns,
                &["id", "project_id", "name"],
                &[
                    "program",
                    "model",
                    "task_description",
                    "inception_ts",
                    "last_active_ts",
                    "attachments_policy",
                    "contact_policy",
                    "reaper_exempt",
                    "registration_token",
                ],
                stats,
                salvage_db_path,
            )
            .ok_or_else(|| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: agents schema is incomplete in {}",
                    salvage_db_path.display()
                ))
            })?;
            let agent_rows = salvage_conn
                .query_sync(
                    &format!("SELECT {agent_select} FROM agents ORDER BY id"),
                    &[],
                )
                .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: query agents: {e}")))?;

            for row in &agent_rows {
                let source_agent_id = row.get_named::<i64>("id").map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: decode agent id: {e}"))
                })?;
                if source_agent_id <= 0 {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: agent has non-positive id {source_agent_id}"
                    )));
                }
                let source_project_id = row.get_named::<i64>("project_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode project_id for agent {source_agent_id}: {e}"
                    ))
                })?;
                let target_project_id = *project_id_map.get(&source_project_id).ok_or_else(|| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: agent {source_agent_id} referenced unmapped project id {source_project_id}"
                    ))
                })?;

                let name = row.get_named::<String>("name").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode name for agent {source_agent_id}: {e}"
                    ))
                })?;
                let name = name.trim().to_string();
                if name.is_empty() {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: agent {source_agent_id} has an empty stable name"
                    )));
                }

                let salvaged_program_raw = row.get_named::<String>("program").ok();
                let salvaged_model_raw = row.get_named::<String>("model").ok();
                let salvaged_task_description = row
                    .get_named::<String>("task_description")
                    .unwrap_or_default();
                let salvaged_inception_ts = row
                    .get_named::<i64>("inception_ts")
                    .unwrap_or_else(|_| crate::now_micros());
                let salvaged_last_active_ts = row
                    .get_named::<i64>("last_active_ts")
                    .unwrap_or_else(|_| crate::now_micros());
                let salvaged_attachments_policy_raw =
                    row.get_named::<String>("attachments_policy").ok();
                let salvaged_contact_policy_raw = row.get_named::<String>("contact_policy").ok();
                let salvaged_reaper_exempt = if agent_columns.contains("reaper_exempt") {
                    Some(
                        row.get_named::<i64>("reaper_exempt")
                            .map_err(|e| {
                                DbError::Sqlite(format!(
                                    "reconstruct salvage: decode reaper_exempt for agent {source_agent_id}: {e}"
                                ))
                            })?
                            != 0,
                    )
                } else {
                    None
                };
                let salvaged_registration_token = if agent_columns.contains("registration_token") {
                    row.get_named::<Option<String>>("registration_token")
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: decode registration_token for agent {source_agent_id}: {e}"
                            ))
                        })?
                } else {
                    None
                };
                let salvage_agent_source = format!("salvage agent row {source_agent_id} ({name})");
                let salvaged_program = normalize_reconstructed_required_agent_field(
                    salvaged_program_raw.as_deref(),
                    &salvage_agent_source,
                    "program",
                    "unknown",
                    stats,
                );
                let salvaged_model = normalize_reconstructed_required_agent_field(
                    salvaged_model_raw.as_deref(),
                    &salvage_agent_source,
                    "model",
                    "unknown",
                    stats,
                );
                let salvaged_attachments_policy = normalize_reconstructed_attachments_policy(
                    salvaged_attachments_policy_raw.as_deref(),
                    &salvage_agent_source,
                    stats,
                );
                let salvaged_contact_policy = normalize_reconstructed_contact_policy(
                    salvaged_contact_policy_raw.as_deref(),
                    &salvage_agent_source,
                    stats,
                );

                let existed = query_last_insert_or_existing_id_composite(
                    &target_conn,
                    "agents",
                    "project_id",
                    target_project_id,
                    "name",
                    &name,
                )
                .ok();

                target_conn
                .execute_sync(
                    "INSERT OR IGNORE INTO agents \
                     (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, registration_token) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    &[
                        Value::BigInt(target_project_id),
                        Value::Text(name.clone()),
                        Value::Text(salvaged_program.clone()),
                        Value::Text(salvaged_model.clone()),
                        Value::Text(salvaged_task_description.clone()),
                        Value::BigInt(salvaged_inception_ts),
                        Value::BigInt(salvaged_last_active_ts),
                        Value::Text(salvaged_attachments_policy.clone()),
                        Value::Text(salvaged_contact_policy.clone()),
                        Value::BigInt(i64::from(salvaged_reaper_exempt.unwrap_or(false))),
                        salvaged_registration_token
                            .clone()
                            .map_or(Value::Null, Value::Text),
                    ],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: insert agent {name}: {e}"))
                })?;

                let target_agent_id = query_last_insert_or_existing_id_composite(
                    &target_conn,
                    "agents",
                    "project_id",
                    target_project_id,
                    "name",
                    &name,
                )?;
                agent_id_map.insert(source_agent_id, target_agent_id);
                if existed.is_none() {
                    stats.salvaged_agents += 1;
                } else {
                    enrich_existing_agent_from_salvage(
                        &target_conn,
                        target_agent_id,
                        &name,
                        &salvaged_program,
                        &salvaged_model,
                        &salvaged_task_description,
                        salvaged_inception_ts,
                        salvaged_last_active_ts,
                        &salvaged_attachments_policy,
                        &salvaged_contact_policy,
                        salvaged_reaper_exempt,
                        salvaged_registration_token.as_deref(),
                        agent_columns.contains("registration_token"),
                        stats,
                    )?;
                }
            }
        }

        if has_file_reservations {
            let reservation_columns = table_columns(&salvage_conn, "file_reservations")?;
            let reservation_select = build_salvage_select(
                "file_reservations",
                &reservation_columns,
                &[
                    "id",
                    "project_id",
                    "agent_id",
                    "path_pattern",
                    "exclusive",
                    "reason",
                    "created_ts",
                    "expires_ts",
                ],
                &["released_ts"],
                stats,
                salvage_db_path,
            )
            .ok_or_else(|| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: file_reservations schema is incomplete in {}",
                    salvage_db_path.display()
                ))
            })?;
            let reservation_rows = salvage_conn
                .query_sync(
                    &format!("SELECT {reservation_select} FROM file_reservations ORDER BY id"),
                    &[],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: query file_reservations: {e}"))
                })?;
            if !reservation_rows.is_empty()
                && (project_id_map.is_empty() || agent_id_map.is_empty())
            {
                return Err(DbError::Sqlite(format!(
                    "reconstruct salvage: {} has file_reservations rows but stable project/agent identity maps are unavailable",
                    salvage_db_path.display()
                )));
            }

            for row in &reservation_rows {
                let source_reservation_id = row.get_named::<i64>("id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode file reservation id: {e}"
                    ))
                })?;
                if source_reservation_id <= 0 {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: file reservation has non-positive id {source_reservation_id}"
                    )));
                }
                let source_project_id = row.get_named::<i64>("project_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode project_id for reservation {source_reservation_id}: {e}"
                    ))
                })?;
                let source_agent_id = row.get_named::<i64>("agent_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode agent_id for reservation {source_reservation_id}: {e}"
                    ))
                })?;
                let target_project_id = *project_id_map.get(&source_project_id).ok_or_else(|| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: reservation {source_reservation_id} referenced unmapped project id {source_project_id}"
                    ))
                })?;
                let target_agent_id = *agent_id_map.get(&source_agent_id).ok_or_else(|| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: reservation {source_reservation_id} referenced unmapped agent id {source_agent_id}"
                    ))
                })?;
                if agent_project_id(&target_conn, target_agent_id)? != Some(target_project_id) {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: reservation {source_reservation_id} maps agent {source_agent_id} outside project {source_project_id}; refusing cross-project ownership"
                    )));
                }

                let path_pattern = row
                    .get_named::<String>("path_pattern")
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode path_pattern for reservation {source_reservation_id}: {e}"
                        ))
                    })?
                    .trim()
                    .to_string();
                if path_pattern.is_empty() {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: reservation {source_reservation_id} has an empty path_pattern"
                    )));
                }
                let exclusive = i64::from(
                    row.get_named::<i64>("exclusive").map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode exclusive for reservation {source_reservation_id}: {e}"
                        ))
                    })? != 0,
                );
                let reason = row.get_named::<String>("reason").unwrap_or_default();
                let created_ts = row.get_named::<i64>("created_ts").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode created_ts for reservation {source_reservation_id}: {e}"
                    ))
                })?;
                let expires_ts = row.get_named::<i64>("expires_ts").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode expires_ts for reservation {source_reservation_id}: {e}"
                    ))
                })?;
                let released_ts = row.get_named::<i64>("released_ts").ok();

                // Numeric ids are local to the source database. Resolve the
                // logical reservation exclusively through remapped stable
                // project/agent identities plus its immutable path/time key.
                let existing_rows = target_conn
                    .query_sync(
                        "SELECT id, exclusive, reason, expires_ts, released_ts \
                         FROM file_reservations \
                         WHERE project_id = ? AND agent_id = ? AND path_pattern = ? AND created_ts = ? \
                         ORDER BY id",
                        &[
                            Value::BigInt(target_project_id),
                            Value::BigInt(target_agent_id),
                            Value::Text(path_pattern.clone()),
                            Value::BigInt(created_ts),
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: resolve reservation {source_reservation_id} by stable identity: {e}"
                        ))
                    })?;
                if existing_rows.len() > 1 {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: reservation {source_reservation_id} has {} target rows for the same stable ownership key; refusing ambiguous promotion",
                        existing_rows.len()
                    )));
                }

                let target_reservation_id = if let Some(existing) = existing_rows.first() {
                    let target_reservation_id = existing.get_named::<i64>("id").map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode target reservation id: {e}"
                        ))
                    })?;
                    let current_exclusive =
                        i64::from(existing.get_named::<i64>("exclusive").unwrap_or(1) != 0);
                    let current_reason = existing.get_named::<String>("reason").unwrap_or_default();
                    let current_expires_ts = existing
                        .get_named::<i64>("expires_ts")
                        .unwrap_or(expires_ts);
                    let current_released_ts = existing.get_named::<i64>("released_ts").ok();
                    if current_exclusive != exclusive {
                        return Err(DbError::Sqlite(format!(
                            "reconstruct salvage: reservation {source_reservation_id} conflicts with target reservation {target_reservation_id} on exclusive ownership for the same stable key"
                        )));
                    }
                    if !current_reason.is_empty() && !reason.is_empty() && current_reason != reason
                    {
                        return Err(DbError::Sqlite(format!(
                            "reconstruct salvage: reservation {source_reservation_id} conflicts with target reservation {target_reservation_id} on reason metadata for the same stable key"
                        )));
                    }
                    if current_released_ts.is_some()
                        && released_ts.is_some()
                        && current_released_ts != released_ts
                    {
                        return Err(DbError::Sqlite(format!(
                            "reconstruct salvage: reservation {source_reservation_id} conflicts with target reservation {target_reservation_id} on terminal release timestamp"
                        )));
                    }
                    let merged_reason = if current_reason.is_empty() {
                        reason.clone()
                    } else {
                        current_reason.clone()
                    };
                    let merged_expires_ts = current_expires_ts.max(expires_ts);
                    let merged_released_ts = current_released_ts.or(released_ts);
                    if merged_reason != current_reason
                        || merged_expires_ts != current_expires_ts
                        || merged_released_ts != current_released_ts
                    {
                        target_conn
                            .execute_sync(
                                "UPDATE file_reservations SET reason = ?, expires_ts = ?, released_ts = ? WHERE id = ?",
                                &[
                                    Value::Text(merged_reason),
                                    Value::BigInt(merged_expires_ts),
                                    merged_released_ts.map_or(Value::Null, Value::BigInt),
                                    Value::BigInt(target_reservation_id),
                                ],
                            )
                            .map_err(|e| {
                                DbError::Sqlite(format!(
                                    "reconstruct salvage: merge reservation {source_reservation_id} state: {e}"
                                ))
                            })?;
                        stats.salvaged_reservations += 1;
                    }
                    target_reservation_id
                } else {
                    target_conn
                        .execute_sync(
                            "INSERT INTO file_reservations \
                             (project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                            &[
                                Value::BigInt(target_project_id),
                                Value::BigInt(target_agent_id),
                                Value::Text(path_pattern),
                                Value::BigInt(exclusive),
                                Value::Text(reason),
                                Value::BigInt(created_ts),
                                Value::BigInt(expires_ts),
                                released_ts.map_or(Value::Null, Value::BigInt),
                            ],
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: insert reservation {source_reservation_id}: {e}"
                            ))
                        })?;
                    stats.salvaged_reservations += 1;
                    query_last_insert_rowid(&target_conn)?
                };
                reservation_id_map.insert(source_reservation_id, target_reservation_id);
            }
        }

        if has_file_reservation_releases {
            if !has_file_reservations {
                return Err(DbError::Sqlite(format!(
                    "reconstruct salvage: {} has file_reservation_releases without file_reservations",
                    salvage_db_path.display()
                )));
            }
            let release_columns = table_columns(&salvage_conn, "file_reservation_releases")?;
            let release_select = build_salvage_select(
                "file_reservation_releases",
                &release_columns,
                &["reservation_id", "released_ts"],
                &[],
                stats,
                salvage_db_path,
            )
            .ok_or_else(|| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: file_reservation_releases schema is incomplete in {}",
                    salvage_db_path.display()
                ))
            })?;
            let release_rows = salvage_conn
                .query_sync(
                    &format!(
                        "SELECT {release_select} FROM file_reservation_releases ORDER BY reservation_id"
                    ),
                    &[],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: query file_reservation_releases: {e}"
                    ))
                })?;
            for row in &release_rows {
                let source_reservation_id =
                    row.get_named::<i64>("reservation_id").map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode release reservation_id: {e}"
                        ))
                    })?;
                let released_ts = row.get_named::<i64>("released_ts").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode release timestamp for reservation {source_reservation_id}: {e}"
                    ))
                })?;
                let target_reservation_id =
                    *reservation_id_map.get(&source_reservation_id).ok_or_else(|| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: release references unmapped reservation id {source_reservation_id}"
                        ))
                    })?;
                let existing_release_rows = target_conn
                    .query_sync(
                        "SELECT released_ts FROM file_reservation_releases WHERE reservation_id = ?",
                        &[Value::BigInt(target_reservation_id)],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: query release for target reservation {target_reservation_id}: {e}"
                        ))
                    })?;
                if let Some(existing) = existing_release_rows.first() {
                    let current_released_ts = existing.get_named::<i64>("released_ts").map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode target release for reservation {target_reservation_id}: {e}"
                        ))
                    })?;
                    if current_released_ts != released_ts {
                        return Err(DbError::Sqlite(format!(
                            "reconstruct salvage: reservation {source_reservation_id} has conflicting terminal release ledger timestamps ({released_ts} versus {current_released_ts})"
                        )));
                    }
                    continue;
                }
                let legacy_release_rows = target_conn
                    .query_sync(
                        "SELECT released_ts FROM file_reservations WHERE id = ?",
                        &[Value::BigInt(target_reservation_id)],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: query legacy release state for reservation {target_reservation_id}: {e}"
                        ))
                    })?;
                if let Some(legacy_release) = legacy_release_rows
                    .first()
                    .and_then(|existing| existing.get_named::<i64>("released_ts").ok())
                    && legacy_release != released_ts
                {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: reservation {source_reservation_id} has conflicting row/ledger release timestamps ({legacy_release} versus {released_ts})"
                    )));
                }
                target_conn
                    .execute_sync(
                        "INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (?, ?)",
                        &[
                            Value::BigInt(target_reservation_id),
                            Value::BigInt(released_ts),
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: insert release for reservation {source_reservation_id}: {e}"
                        ))
                    })?;
                stats.salvaged_reservation_releases += 1;
            }
        }

        if has_agent_links {
            let agent_link_columns = table_columns(&salvage_conn, "agent_links")?;
            let agent_link_select =
                build_salvage_agent_links_select(&agent_link_columns, stats, salvage_db_path)
                    .ok_or_else(|| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: agent_links schema is incomplete in {}",
                            salvage_db_path.display()
                        ))
                    })?;
            let agent_link_rows = salvage_conn
                .query_sync(
                    &format!(
                        "SELECT {agent_link_select} FROM agent_links \
                         ORDER BY a_project_id, a_agent_id, b_project_id, b_agent_id"
                    ),
                    &[],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: query agent_links: {e}"))
                })?;
            if !agent_link_rows.is_empty() && (project_id_map.is_empty() || agent_id_map.is_empty())
            {
                return Err(DbError::Sqlite(format!(
                    "reconstruct salvage: {} has agent_links rows but stable project/agent identity maps are unavailable",
                    salvage_db_path.display()
                )));
            }

            for row in &agent_link_rows {
                let source_origin_project_id =
                    row.get_named::<i64>("a_project_id").map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode agent_link origin project: {e}"
                        ))
                    })?;
                let source_origin_agent_id = row.get_named::<i64>("a_agent_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode agent_link origin agent: {e}"
                    ))
                })?;
                let source_peer_project_id = row.get_named::<i64>("b_project_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode agent_link peer project: {e}"
                    ))
                })?;
                let source_peer_agent_id = row.get_named::<i64>("b_agent_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode agent_link peer agent: {e}"
                    ))
                })?;
                let target_origin_project_id = *project_id_map
                    .get(&source_origin_project_id)
                    .ok_or_else(|| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: agent_link references unmapped origin project {source_origin_project_id}"
                        ))
                    })?;
                let target_origin_agent_id = *agent_id_map
                    .get(&source_origin_agent_id)
                    .ok_or_else(|| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: agent_link references unmapped origin agent {source_origin_agent_id}"
                        ))
                    })?;
                let target_peer_project_id = *project_id_map
                    .get(&source_peer_project_id)
                    .ok_or_else(|| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: agent_link references unmapped peer project {source_peer_project_id}"
                        ))
                    })?;
                let target_peer_agent_id =
                    *agent_id_map.get(&source_peer_agent_id).ok_or_else(|| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: agent_link references unmapped peer agent {source_peer_agent_id}"
                        ))
                    })?;
                if agent_project_id(&target_conn, target_origin_agent_id)?
                    != Some(target_origin_project_id)
                    || agent_project_id(&target_conn, target_peer_agent_id)?
                        != Some(target_peer_project_id)
                {
                    return Err(DbError::Sqlite(
                        "reconstruct salvage: agent_link ownership crosses a stable project boundary"
                            .to_string(),
                    ));
                }

                let link_status = row
                    .get_named::<String>("status")
                    .unwrap_or_else(|_| "pending".to_string());
                let reason = row.get_named::<String>("reason").unwrap_or_default();
                let created_ts = row
                    .get_named::<i64>("created_ts")
                    .unwrap_or_else(|_| crate::now_micros());
                let updated_ts = row.get_named::<i64>("updated_ts").unwrap_or(created_ts);
                let expires_ts = row.get_named::<i64>("expires_ts").ok();
                if created_ts <= 0 || updated_ts < created_ts {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: agent_link {source_origin_project_id}/{source_origin_agent_id}->{source_peer_project_id}/{source_peer_agent_id} has invalid timestamp ordering ({created_ts}, {updated_ts})"
                    )));
                }

                let existing_links = target_conn
                    .query_sync(
                        "SELECT id FROM agent_links \
                         WHERE a_project_id = ? AND a_agent_id = ? \
                           AND b_project_id = ? AND b_agent_id = ? LIMIT 2",
                        &[
                            Value::BigInt(target_origin_project_id),
                            Value::BigInt(target_origin_agent_id),
                            Value::BigInt(target_peer_project_id),
                            Value::BigInt(target_peer_agent_id),
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: query existing agent_link: {e}"
                        ))
                    })?;
                if existing_links.len() > 1 {
                    return Err(DbError::Sqlite(
                        "reconstruct salvage: multiple target agent_links share the same stable endpoint quartet"
                            .to_string(),
                    ));
                }
                let state_values = [
                    Value::Text(link_status),
                    Value::Text(reason),
                    Value::BigInt(created_ts),
                    Value::BigInt(updated_ts),
                    expires_ts.map_or(Value::Null, Value::BigInt),
                ];
                if let Some(existing) = existing_links.first() {
                    let target_link_id = existing.get_named::<i64>("id").map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode existing agent_link id: {e}"
                        ))
                    })?;
                    let mut values = state_values.to_vec();
                    values.push(Value::BigInt(target_link_id));
                    target_conn
                        .execute_sync(
                            "UPDATE agent_links SET status = ?, reason = ?, created_ts = ?, updated_ts = ?, expires_ts = ? WHERE id = ?",
                            &values,
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: restore state for agent_link {target_link_id}: {e}"
                            ))
                        })?;
                } else {
                    let mut values = vec![
                        Value::BigInt(target_origin_project_id),
                        Value::BigInt(target_origin_agent_id),
                        Value::BigInt(target_peer_project_id),
                        Value::BigInt(target_peer_agent_id),
                    ];
                    values.extend(state_values);
                    target_conn
                        .execute_sync(
                            "INSERT INTO agent_links \
                             (a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts, expires_ts) \
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                            &values,
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: insert agent_link {source_origin_project_id}/{source_origin_agent_id}->{source_peer_project_id}/{source_peer_agent_id}: {e}"
                            ))
                        })?;
                }
            }
        }

        if has_products {
            let product_columns = table_columns(&salvage_conn, "products")?;
            let product_select = build_salvage_select(
                "products",
                &product_columns,
                &["id", "product_uid", "name"],
                &["created_at"],
                stats,
                salvage_db_path,
            )
            .ok_or_else(|| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: products schema is incomplete in {}",
                    salvage_db_path.display()
                ))
            })?;
            let product_rows = salvage_conn
                .query_sync(
                    &format!("SELECT {product_select} FROM products ORDER BY id"),
                    &[],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: query products: {e}"))
                })?;

            for row in &product_rows {
                let source_product_id = row.get_named::<i64>("id").map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: decode product id: {e}"))
                })?;
                if source_product_id <= 0 {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: product has non-positive id {source_product_id}"
                    )));
                }
                let product_uid = row.get_named::<String>("product_uid").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode uid for product {source_product_id}: {e}"
                    ))
                })?;
                let product_uid = product_uid.trim().to_string();
                if product_uid.is_empty() {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: product {source_product_id} has an empty stable uid"
                    )));
                }
                let name = row.get_named::<String>("name").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode name for product {source_product_id}: {e}"
                    ))
                })?;
                let name = name.trim().to_string();
                if name.is_empty() {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: product {source_product_id} has an empty name"
                    )));
                }

                let uid_rows = target_conn
                    .query_sync(
                        "SELECT id, name FROM products WHERE product_uid = ? LIMIT 2",
                        &[Value::Text(product_uid.clone())],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: query product uid {product_uid}: {e}"
                        ))
                    })?;
                let name_rows = target_conn
                    .query_sync(
                        "SELECT id, product_uid FROM products WHERE name = ? LIMIT 2",
                        &[Value::Text(name.clone())],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: query product name {name:?}: {e}"
                        ))
                    })?;

                let target_product_id = if let Some(existing) = uid_rows.first() {
                    let existing_id = existing.get_named::<i64>("id").map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode existing product {product_uid} id: {e}"
                        ))
                    })?;
                    let existing_name = existing.get_named::<String>("name").map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode existing product {product_uid} name: {e}"
                        ))
                    })?;
                    if existing_name.trim() != name {
                        return Err(DbError::Sqlite(format!(
                            "reconstruct salvage: stable product uid {product_uid:?} has conflicting names {:?} and {name:?}; refusing ambiguous product identity",
                            existing_name.trim()
                        )));
                    }
                    if let Some(named) = name_rows.first() {
                        let named_id = named.get_named::<i64>("id").map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: decode product name {name:?} id: {e}"
                            ))
                        })?;
                        if named_id != existing_id {
                            return Err(DbError::Sqlite(format!(
                                "reconstruct salvage: product uid {product_uid:?} and name {name:?} resolve to different target rows; refusing cross-binding"
                            )));
                        }
                    }
                    existing_id
                } else {
                    if let Some(existing) = name_rows.first() {
                        let existing_uid = existing
                            .get_named::<String>("product_uid")
                            .unwrap_or_default();
                        return Err(DbError::Sqlite(format!(
                            "reconstruct salvage: product name {name:?} is already bound to stable uid {:?}, not {product_uid:?}; refusing name-based identity fallback",
                            existing_uid.trim()
                        )));
                    }
                    target_conn
                        .execute_sync(
                            "INSERT INTO products (product_uid, name, created_at) VALUES (?, ?, ?)",
                            &[
                                Value::Text(product_uid.clone()),
                                Value::Text(name.clone()),
                                Value::BigInt(
                                    row.get_named::<i64>("created_at")
                                        .unwrap_or_else(|_| crate::now_micros()),
                                ),
                            ],
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: insert product {product_uid}: {e}"
                            ))
                        })?;
                    query_last_insert_or_existing_id(
                        &target_conn,
                        "products",
                        "product_uid",
                        &product_uid,
                    )?
                };
                product_id_map.insert(source_product_id, target_product_id);
            }
        }

        if has_product_project_links {
            let product_link_columns = table_columns(&salvage_conn, "product_project_links")?;
            let product_link_select = build_salvage_select(
                "product_project_links",
                &product_link_columns,
                &["product_id", "project_id"],
                &["created_at"],
                stats,
                salvage_db_path,
            )
            .ok_or_else(|| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: product_project_links schema is incomplete in {}",
                    salvage_db_path.display()
                ))
            })?;
            let product_link_rows = salvage_conn
                .query_sync(
                    &format!(
                        "SELECT {product_link_select} FROM product_project_links \
                             ORDER BY product_id, project_id"
                    ),
                    &[],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: query product_project_links: {e}"
                    ))
                })?;
            if !product_link_rows.is_empty()
                && (product_id_map.is_empty() || project_id_map.is_empty())
            {
                return Err(DbError::Sqlite(format!(
                    "reconstruct salvage: {} has product_project_links rows but stable product/project identity maps are unavailable",
                    salvage_db_path.display()
                )));
            }

            for row in &product_link_rows {
                let source_product_id = row.get_named::<i64>("product_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode product_project_link product id: {e}"
                    ))
                })?;
                let source_project_id = row.get_named::<i64>("project_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode product_project_link project id: {e}"
                    ))
                })?;
                let target_product_id = *product_id_map
                            .get(&source_product_id)
                            .ok_or_else(|| {
                                DbError::Sqlite(format!(
                                    "reconstruct salvage: product_project_link references unmapped product {source_product_id}"
                                ))
                            })?;
                let target_project_id = *project_id_map
                            .get(&source_project_id)
                            .ok_or_else(|| {
                                DbError::Sqlite(format!(
                                    "reconstruct salvage: product_project_link references unmapped project {source_project_id}"
                                ))
                            })?;

                target_conn
                        .execute_sync(
                            "INSERT OR IGNORE INTO product_project_links (product_id, project_id, created_at) VALUES (?, ?, ?)",
                            &[
                                Value::BigInt(target_product_id),
                                Value::BigInt(target_project_id),
                                Value::BigInt(
                                    row.get_named::<i64>("created_at")
                                        .unwrap_or_else(|_| crate::now_micros()),
                                ),
                            ],
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: insert product_project_link \
                                 {source_product_id}->{source_project_id}: {e}"
                            ))
                        })?;
            }
        }

        if has_proof_gate_consumed_nonces {
            let nonce_columns = table_columns(&salvage_conn, "proof_gate_consumed_nonces")?;
            let nonce_select = build_salvage_select(
                "proof_gate_consumed_nonces",
                &nonce_columns,
                &["issuer_key", "nonce", "retain_until", "consumed_at"],
                &[],
                stats,
                salvage_db_path,
            )
            .ok_or_else(|| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: proof_gate_consumed_nonces schema is incomplete in {}",
                    salvage_db_path.display()
                ))
            })?;
            let nonce_rows = salvage_conn
                .query_sync(
                    &format!(
                        "SELECT {nonce_select} FROM proof_gate_consumed_nonces ORDER BY issuer_key, nonce"
                    ),
                    &[],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: query proof_gate_consumed_nonces: {e}"
                    ))
                })?;

            for row in &nonce_rows {
                let issuer_key = row
                    .get_named::<String>("issuer_key")
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode proof nonce issuer key: {e}"
                        ))
                    })?
                    .trim()
                    .to_string();
                let nonce = row
                    .get_named::<String>("nonce")
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode proof nonce value: {e}"
                        ))
                    })?
                    .trim()
                    .to_string();
                if issuer_key.is_empty() || nonce.is_empty() {
                    return Err(DbError::Sqlite(
                        "reconstruct salvage: consumed proof nonce has an empty stable issuer/nonce key"
                            .to_string(),
                    ));
                }
                let retain_until = row.get_named::<i64>("retain_until").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode retain_until for proof nonce: {e}"
                    ))
                })?;
                let consumed_at = row.get_named::<i64>("consumed_at").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode consumed_at for proof nonce: {e}"
                    ))
                })?;
                let existing = target_conn
                    .query_sync(
                        "SELECT retain_until, consumed_at FROM proof_gate_consumed_nonces \
                         WHERE issuer_key = ? AND nonce = ? LIMIT 2",
                        &[Value::Text(issuer_key.clone()), Value::Text(nonce.clone())],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: query existing consumed proof nonce: {e}"
                        ))
                    })?;
                if let Some(existing) = existing.first() {
                    let current_retain_until = existing
                        .get_named::<i64>("retain_until")
                        .unwrap_or_default();
                    let current_consumed_at =
                        existing.get_named::<i64>("consumed_at").unwrap_or_default();
                    if current_retain_until != retain_until || current_consumed_at != consumed_at {
                        return Err(DbError::Sqlite(format!(
                            "reconstruct salvage: consumed proof nonce ({issuer_key:?}, {nonce:?}) has conflicting durable timestamps; refusing to weaken replay prevention"
                        )));
                    }
                    continue;
                }
                target_conn
                    .execute_sync(
                        "INSERT INTO proof_gate_consumed_nonces \
                         (issuer_key, nonce, retain_until, consumed_at) VALUES (?, ?, ?, ?)",
                        &[
                            Value::Text(issuer_key),
                            Value::Text(nonce),
                            Value::BigInt(retain_until),
                            Value::BigInt(consumed_at),
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: insert consumed proof nonce: {e}"
                        ))
                    })?;
            }
        }

        let mut reconstructed_recipient_agent_ids: HashMap<(i64, String), i64> = HashMap::new();
        let mut recipient_json_updates = BTreeSet::new();

        if has_messages {
            let message_columns = table_columns(&salvage_conn, "messages")?;
            let message_select = build_salvage_select(
                "messages",
                &message_columns,
                &["id", "project_id", "sender_id"],
                &[
                    "thread_id",
                    "subject",
                    "body_md",
                    "importance",
                    "ack_required",
                    "created_ts",
                    "recipients_json",
                    "attachments",
                ],
                stats,
                salvage_db_path,
            )
            .ok_or_else(|| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: messages schema is incomplete in {}",
                    salvage_db_path.display()
                ))
            })?;
            #[cfg(test)]
            if FAIL_SALVAGE_QUERY_MESSAGES.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(DbError::Sqlite(
                    "reconstruct salvage: query messages: Query error: database disk image is malformed"
                        .to_owned(),
                ));
            }

            let message_rows = salvage_conn
                .query_sync(
                    &format!("SELECT {message_select} FROM messages ORDER BY id"),
                    &[],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: query messages: {e}"))
                })?;

            for row in &message_rows {
                let source_message_id = row.get_named::<i64>("id").map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: decode message id: {e}"))
                })?;
                if source_message_id <= 0 {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: message has non-positive id {source_message_id}"
                    )));
                }
                let source_project_id = row.get_named::<i64>("project_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode project_id for message {source_message_id}: {e}"
                    ))
                })?;
                let target_project_id = *project_id_map.get(&source_project_id).ok_or_else(|| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: message {source_message_id} referenced unmapped project id {source_project_id}"
                    ))
                })?;
                let source_sender_id = row.get_named::<i64>("sender_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode sender_id for message {source_message_id}: {e}"
                    ))
                })?;
                let target_sender_id = *agent_id_map.get(&source_sender_id).ok_or_else(|| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: message {source_message_id} referenced unmapped sender id {source_sender_id}"
                    ))
                })?;
                if agent_project_id(&target_conn, target_sender_id)? != Some(target_project_id) {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: message {source_message_id} maps sender {source_sender_id} outside project {source_project_id}; refusing cross-project ownership"
                    )));
                }

                if message_project_id(&target_conn, source_message_id)? == Some(target_project_id) {
                    message_id_map.insert(source_message_id, source_message_id);
                    continue;
                }

                let thread_id = row
                    .get_named::<String>("thread_id")
                    .ok()
                    .and_then(|raw: String| sanitize_reconstructed_thread_id(raw.as_str()));
                let thread_value = thread_id.map_or(Value::Null, Value::Text);
                let (recipients_json, to_names, cc_names, bcc_names) =
                    parse_salvaged_recipients_json(
                        row.get_named::<String>("recipients_json").ok(),
                        source_message_id,
                        stats,
                    );
                let attachments = parse_salvaged_attachments_json(
                    row.get_named::<String>("attachments").ok(),
                    source_message_id,
                    stats,
                );
                let values = [
                    Value::BigInt(target_project_id),
                    Value::BigInt(target_sender_id),
                    thread_value,
                    Value::Text(row.get_named::<String>("subject").unwrap_or_default()),
                    Value::Text(row.get_named::<String>("body_md").unwrap_or_default()),
                    Value::Text(
                        row.get_named::<String>("importance")
                            .unwrap_or_else(|_| "normal".to_string()),
                    ),
                    Value::BigInt(i64::from(
                        row.get_named::<i64>("ack_required").unwrap_or(0) != 0,
                    )),
                    Value::BigInt(
                        row.get_named::<i64>("created_ts")
                            .unwrap_or_else(|_| crate::now_micros()),
                    ),
                    Value::Text(recipients_json),
                    Value::Text(attachments),
                ];
                let existing_project_id = message_project_id(&target_conn, source_message_id)?;
                let target_message_id = if let Some(existing_project_id) = existing_project_id {
                    target_conn
                        .execute_sync(
                            "INSERT INTO messages \
                             (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                            &values,
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: remap cross-project message {source_message_id}: {e}"
                            ))
                        })?;
                    let remapped_id = query_last_insert_rowid(&target_conn)?;
                    stats.salvaged_message_id_remaps += 1;
                    stats.warnings.push(format!(
                        "Salvage message id {source_message_id} belonged to remapped project {target_project_id}, but the archive candidate already used that numeric id for project {existing_project_id}; preserved it as message {remapped_id}"
                    ));
                    remapped_id
                } else {
                    let mut values_with_id = Vec::with_capacity(values.len() + 1);
                    values_with_id.push(Value::BigInt(source_message_id));
                    values_with_id.extend(values);
                    target_conn
                        .execute_sync(
                            "INSERT INTO messages \
                             (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                            &values_with_id,
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: insert message {source_message_id}: {e}"
                            ))
                        })?;
                    source_message_id
                };
                message_id_map.insert(source_message_id, target_message_id);
                stats.salvaged_messages += 1;

                for (names, kind) in [(&to_names, "to"), (&cc_names, "cc"), (&bcc_names, "bcc")] {
                    for name in names {
                        let agent_id = ensure_agent_exists(
                            &target_conn,
                            target_project_id,
                            name,
                            &mut reconstructed_recipient_agent_ids,
                        )?;
                        insert_recipient(&target_conn, target_message_id, agent_id, kind)?;
                        stats.salvaged_recipients += 1;
                        recipient_json_updates.insert(target_message_id);
                    }
                }
            }
        }

        if has_recipients {
            let recipient_columns = table_columns(&salvage_conn, "message_recipients")?;
            let recipient_select = build_salvage_select(
                "message_recipients",
                &recipient_columns,
                &["message_id", "agent_id", "kind"],
                &["read_ts", "ack_ts"],
                stats,
                salvage_db_path,
            )
            .ok_or_else(|| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: message_recipients schema is incomplete in {}",
                    salvage_db_path.display()
                ))
            })?;
            let recipient_rows = salvage_conn
                .query_sync(
                &format!(
                    "SELECT {recipient_select} FROM message_recipients ORDER BY message_id, agent_id, kind"
                ),
                &[],
            )
            .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: query recipients: {e}")))?;

            for row in &recipient_rows {
                let source_message_id = row.get_named::<i64>("message_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode recipient message_id: {e}"
                    ))
                })?;
                let source_agent_id = row.get_named::<i64>("agent_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode agent_id for message {source_message_id}: {e}"
                    ))
                })?;
                let target_agent_id = *agent_id_map.get(&source_agent_id).ok_or_else(|| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: recipient for message {source_message_id} references unmapped agent id {source_agent_id}"
                    ))
                })?;
                let target_agent_project_id = agent_project_id(&target_conn, target_agent_id)?
                    .ok_or_else(|| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: mapped target agent {target_agent_id} is missing"
                        ))
                    })?;
                let target_message_id = *message_id_map.get(&source_message_id).ok_or_else(|| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: recipient references unmapped source-local message id {source_message_id}; refusing to attach state without a decoded salvage message identity"
                    ))
                })?;
                let target_message_project_id = message_project_id(
                    &target_conn,
                    target_message_id,
                )?
                .ok_or_else(|| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: mapped target message {target_message_id} is missing"
                    ))
                })?;
                if target_agent_project_id != target_message_project_id {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: recipient agent {source_agent_id} for message {source_message_id} maps outside the message project; refusing cross-project recipient state"
                    )));
                }
                let raw_kind = row.get_named::<String>("kind").ok();
                let kind = normalize_salvaged_recipient_kind(
                    raw_kind.as_deref(),
                    target_message_id,
                    stats,
                );
                let read_ts = row.get_named::<i64>("read_ts").ok();
                let ack_ts = row.get_named::<i64>("ack_ts").ok();
                recipient_json_updates.insert(target_message_id);

                let existing_rows = target_conn
                    .query_sync(
                        "SELECT kind, read_ts, ack_ts FROM message_recipients \
                         WHERE message_id = ? AND agent_id = ? LIMIT 2",
                        &[
                            Value::BigInt(target_message_id),
                            Value::BigInt(target_agent_id),
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: query recipient state for message {source_message_id}->{target_message_id}: {e}"
                        ))
                    })?;

                if existing_rows.len() > 1 {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: message {target_message_id} and agent {target_agent_id} have multiple rows despite their stable recipient primary key"
                    )));
                }

                if existing_rows.is_empty() {
                    target_conn
                        .execute_sync(
                            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) \
                             VALUES (?, ?, ?, ?, ?)",
                            &[
                                Value::BigInt(target_message_id),
                                Value::BigInt(target_agent_id),
                                Value::Text(kind),
                                read_ts.map_or(Value::Null, Value::BigInt),
                                ack_ts.map_or(Value::Null, Value::BigInt),
                            ],
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: insert recipient for message {source_message_id}->{target_message_id}: {e}"
                            ))
                        })?;
                    stats.salvaged_recipients += 1;
                    continue;
                }

                let existing_row = &existing_rows[0];
                let current_kind = existing_row.get_named::<String>("kind").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode recipient kind for message {target_message_id}: {e}"
                    ))
                })?;
                if current_kind != kind {
                    return Err(DbError::Sqlite(format!(
                        "reconstruct salvage: recipient ({target_message_id}, {target_agent_id}) has conflicting kinds {current_kind:?} and {kind:?}; refusing a primary-key collision"
                    )));
                }
                let current_read_ts = existing_row
                    .get_named::<Option<i64>>("read_ts")
                    .unwrap_or_default();
                let current_ack_ts = existing_row
                    .get_named::<Option<i64>>("ack_ts")
                    .unwrap_or_default();
                if current_read_ts != read_ts || current_ack_ts != ack_ts {
                    target_conn
                        .execute_sync(
                            "UPDATE message_recipients SET \
                                 read_ts = ?, ack_ts = ? \
                             WHERE message_id = ? AND agent_id = ?",
                            &[
                                read_ts.map_or(Value::Null, Value::BigInt),
                                ack_ts.map_or(Value::Null, Value::BigInt),
                                Value::BigInt(target_message_id),
                                Value::BigInt(target_agent_id),
                            ],
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: update recipient state for message {source_message_id}->{target_message_id}: {e}"
                            ))
                        })?;
                    stats.salvaged_recipients += 1;
                }
            }
        }

        for message_id in recipient_json_updates {
            sync_reconstructed_message_recipients_json(&target_conn, message_id)?;
        }

        // ATC telemetry now lives in the independent sidecar DB (atc.sqlite3),
        // which salvage/reconstruct never replaces (br-bvq1x.11.7). The rebuilt
        // primary mailbox DB has no atc_* tables, so there is nothing to salvage
        // here; the sidecar's rollups persist untouched across recovery and ATC
        // telemetry is, by design, droppable/resettable. `rollups_salvaged`
        // therefore stays 0.

        let cross_project_reservations = target_conn
            .query_sync(
                "SELECT fr.id AS id \
                 FROM file_reservations fr \
                 JOIN agents a ON a.id = fr.agent_id \
                 WHERE fr.project_id <> a.project_id LIMIT 1",
                &[],
            )
            .map_err(|e| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: verify reservation ownership: {e}"
                ))
            })?;
        if let Some(row) = cross_project_reservations.first() {
            let reservation_id = row.get_named::<i64>("id").unwrap_or_default();
            return Err(DbError::Sqlite(format!(
                "reconstruct salvage: reservation {reservation_id} is attached to an agent from another project; refusing promotion"
            )));
        }

        let cross_project_recipients = target_conn
            .query_sync(
                "SELECT mr.message_id AS message_id, mr.agent_id AS agent_id \
                 FROM message_recipients mr \
                 JOIN messages m ON m.id = mr.message_id \
                 JOIN agents a ON a.id = mr.agent_id \
                 WHERE m.project_id <> a.project_id LIMIT 1",
                &[],
            )
            .map_err(|e| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: verify recipient ownership: {e}"
                ))
            })?;
        if let Some(row) = cross_project_recipients.first() {
            let message_id = row.get_named::<i64>("message_id").unwrap_or_default();
            let agent_id = row.get_named::<i64>("agent_id").unwrap_or_default();
            return Err(DbError::Sqlite(format!(
                "reconstruct salvage: recipient agent {agent_id} is attached to message {message_id} from another project; refusing promotion"
            )));
        }

        let foreign_key_failures = target_conn
            .query_sync("PRAGMA foreign_key_check", &[])
            .map_err(|e| {
                DbError::Sqlite(format!(
                    "reconstruct salvage: run post-merge foreign_key_check: {e}"
                ))
            })?;
        if !foreign_key_failures.is_empty() {
            return Err(DbError::Sqlite(format!(
                "reconstruct salvage: post-merge foreign_key_check reported {} violation(s); refusing promotion",
                foreign_key_failures.len()
            )));
        }

        target_conn
            .execute_raw("REINDEX;")
            .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: REINDEX: {e}")))?;
        Ok(())
    })();

    if let Err(err) = merge_result {
        let _ = target_conn.execute_raw("ROLLBACK;");
        *stats = pre_merge_stats;
        return Err(err);
    }
    if let Err(e) = target_conn.execute_raw("COMMIT;") {
        let _ = target_conn.execute_raw("ROLLBACK;");
        *stats = pre_merge_stats;
        return Err(DbError::Sqlite(format!(
            "reconstruct salvage: commit transaction: {e}"
        )));
    }
    drop(target_conn);
    if let Err(e) = crate::pool::wal_checkpoint_truncate_path(target_db_path) {
        stats.warnings.push(format!(
            "Salvage merge committed, but WAL checkpoint failed for {}: {e}",
            target_db_path.display()
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Load canonical `human_key` from `project.json` when available.
///
/// Falls back to a synthetic `/{slug}` placeholder when metadata is missing or
/// malformed. Recovery flows that have a readable salvage database will later
/// replace this placeholder with the canonical path.
fn read_project_human_key(project_path: &Path, slug: &str, stats: &mut ReconstructStats) -> String {
    let metadata_path = project_path.join("project.json");
    let fallback = synthetic_project_placeholder_human_key(slug);

    if !is_real_file(&metadata_path) {
        stats.warnings.push(format!(
            "Missing {}; using fallback human_key '{}'",
            metadata_path.display(),
            fallback
        ));
        return fallback;
    }

    let metadata_str = match read_archive_text_capped(&metadata_path) {
        Ok(s) => s,
        Err(e) => {
            stats.parse_errors += 1;
            stats.warnings.push(format!(
                "Cannot read {}: {e}; using fallback human_key '{}'",
                metadata_path.display(),
                fallback
            ));
            return fallback;
        }
    };

    let metadata_json: serde_json::Value = match serde_json::from_str(&metadata_str) {
        Ok(v) => v,
        Err(e) => {
            stats.parse_errors += 1;
            stats.warnings.push(format!(
                "Cannot parse {}: {e}; using fallback human_key '{}'",
                metadata_path.display(),
                fallback
            ));
            return fallback;
        }
    };

    let Some(human_key) = metadata_json
        .get("human_key")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        stats.parse_errors += 1;
        stats.warnings.push(format!(
            "Missing/empty human_key in {}; using fallback human_key '{}'",
            metadata_path.display(),
            fallback
        ));
        return fallback;
    };

    if !Path::new(human_key).is_absolute() {
        stats.parse_errors += 1;
        stats.warnings.push(format!(
            "Non-absolute human_key '{}' in {}; using fallback human_key '{}'",
            human_key,
            metadata_path.display(),
            fallback
        ));
        return fallback;
    }

    if let Some(metadata_slug) = metadata_json
        .get("slug")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && metadata_slug != slug
    {
        stats.warnings.push(format!(
            "Project metadata slug mismatch in {}: dir slug='{}', metadata slug='{}'",
            metadata_path.display(),
            slug,
            metadata_slug
        ));
    }

    human_key.to_string()
}

fn frontmatter_bounds(content: &str) -> Option<(usize, usize, usize)> {
    let start = content.find("---json")?;
    let after_start = &content[start..];
    let json_start = if after_start.starts_with("---json\r\n") {
        start + "---json\r\n".len()
    } else if after_start.starts_with("---json\n") {
        start + "---json\n".len()
    } else {
        return None;
    };

    let mut search_from = json_start;
    while let Some(relative) = content[search_from..].find("---") {
        let marker_start = search_from + relative;
        if marker_start == 0 || !content[..marker_start].ends_with('\n') {
            search_from = marker_start + 3;
            continue;
        }

        let after_marker = marker_start + 3;
        if after_marker == content.len() {
            return Some((json_start, marker_start, after_marker));
        }
        if content[after_marker..].starts_with("\r\n") {
            return Some((json_start, marker_start, after_marker + 2));
        }
        if content[after_marker..].starts_with('\n') {
            return Some((json_start, marker_start, after_marker + 1));
        }

        search_from = marker_start + 3;
    }

    None
}

/// Extract JSON frontmatter from a `---json\n...\n---` block.
fn extract_json_frontmatter(content: &str) -> Option<&str> {
    let (json_start, json_end, _) = frontmatter_bounds(content)?;
    Some(&content[json_start..json_end])
}

/// Extract the body text after the frontmatter block.
///
/// Only strips leading blank lines; trailing whitespace is preserved
/// so reconstructed bodies match the original archive content.
fn extract_body_after_frontmatter(content: &str) -> Option<&str> {
    let (_, _, body_start) = frontmatter_bounds(content)?;
    let after = &content[body_start..];
    // Skip leading blank lines only — preserve trailing whitespace
    Some(after.trim_start_matches(['\n', '\r']))
}

fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

fn normalized_archive_agent_name(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn normalize_reconstructed_required_agent_field(
    raw: Option<&str>,
    source: &str,
    field: &str,
    fallback: &str,
    stats: &mut ReconstructStats,
) -> String {
    let Some(raw) = raw else {
        return fallback.to_string();
    };
    let normalized = raw.trim();
    if normalized.is_empty() {
        stats.warnings.push(format!(
            "Reconstruct {source} had empty {field}; defaulting to {fallback:?}"
        ));
        fallback.to_string()
    } else {
        normalized.to_string()
    }
}

fn normalize_reconstructed_attachments_policy(
    raw: Option<&str>,
    source: &str,
    stats: &mut ReconstructStats,
) -> String {
    let Some(raw) = raw else {
        return "auto".to_string();
    };
    let normalized = raw.trim().to_ascii_lowercase();
    if VALID_RECONSTRUCTED_ATTACHMENTS_POLICIES.contains(&normalized.as_str()) {
        normalized
    } else {
        stats.warnings.push(format!(
            "Reconstruct {source} had invalid attachments_policy {raw:?}; defaulting to \"auto\""
        ));
        "auto".to_string()
    }
}

fn normalize_reconstructed_contact_policy(
    raw: Option<&str>,
    source: &str,
    stats: &mut ReconstructStats,
) -> String {
    let Some(raw) = raw else {
        return "auto".to_string();
    };
    let normalized = raw.replace('\0', "").trim().to_ascii_lowercase();
    if VALID_RECONSTRUCTED_CONTACT_POLICIES.contains(&normalized.as_str()) {
        normalized
    } else {
        stats.warnings.push(format!(
            "Reconstruct {source} had invalid contact_policy {raw:?}; defaulting to \"auto\""
        ));
        "auto".to_string()
    }
}

fn json_str_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    match value.get(key) {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        Some(serde_json::Value::String(s)) => {
            normalized_archive_agent_name(Some(s)).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

fn reconstructed_recipient_field_is_valid(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(values) => values.iter().all(serde_json::Value::is_string),
        serde_json::Value::String(_) | serde_json::Value::Null => true,
        _ => false,
    }
}

fn reconstructed_recipients_payload_is_valid(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    ["to", "cc", "bcc"].iter().all(|key| {
        object
            .get(*key)
            .is_none_or(reconstructed_recipient_field_is_valid)
    })
}

/// Parse a timestamp field from JSON (supports both ISO string and i64 micros).
fn parse_ts_from_json(value: &serde_json::Value, key: &str) -> Option<i64> {
    match value.get(key)? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            // Try parsing as i64 first (microseconds)
            if let Ok(n) = s.parse::<i64>() {
                return Some(n);
            }
            // Try ISO-8601
            crate::iso_to_micros(s)
        }
        _ => None,
    }
}

/// Query the ID of a row by a unique text column, or the last inserted row.
fn query_last_insert_or_existing_id(
    conn: &DbConn,
    table: &str,
    column: &str,
    value: &str,
) -> DbResult<i64> {
    let rows = conn
        .query_sync(
            &format!("SELECT id FROM {table} WHERE {column} = ?"),
            &[Value::Text(value.to_string())],
        )
        .map_err(|e| DbError::Sqlite(format!("query {table}.id: {e}")))?;

    extract_id_from_rows(&rows)
        .ok_or_else(|| DbError::Sqlite(format!("no id found for {table}.{column} = {value}")))
}

/// Query the ID of a row by a composite key (integer + text).
fn query_last_insert_or_existing_id_composite(
    conn: &DbConn,
    table: &str,
    col1: &str,
    val1: i64,
    col2: &str,
    val2: &str,
) -> DbResult<i64> {
    let rows = conn
        .query_sync(
            &format!("SELECT id FROM {table} WHERE {col1} = ? AND {col2} = ? COLLATE NOCASE"),
            &[Value::BigInt(val1), Value::Text(val2.to_string())],
        )
        .map_err(|e| DbError::Sqlite(format!("query {table}.id composite: {e}")))?;

    extract_id_from_rows(&rows).ok_or_else(|| {
        DbError::Sqlite(format!(
            "no id found for {table}.{col1}={val1}, {col2}={val2}"
        ))
    })
}

/// Get the rowid of the most recently inserted row on this connection.
fn query_last_insert_rowid(conn: &DbConn) -> DbResult<i64> {
    let rows = conn
        .query_sync("SELECT last_insert_rowid() AS id", &[])
        .map_err(|e| DbError::Sqlite(format!("query last_insert_rowid: {e}")))?;

    extract_id_from_rows(&rows)
        .ok_or_else(|| DbError::Sqlite("last_insert_rowid() returned no rows".to_string()))
}

fn extract_id_from_rows(rows: &[sqlmodel_core::Row]) -> Option<i64> {
    let row = rows.first()?;
    match row.get_by_name("id") {
        Some(Value::BigInt(n)) => Some(*n),
        Some(Value::Int(n)) => Some(i64::from(*n)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_one_recipients_json(conn: &DbConn) -> serde_json::Value {
        let rows = conn
            .query_sync("SELECT recipients_json FROM messages WHERE id = 1", &[])
            .unwrap();
        serde_json::from_str(&rows[0].get_named::<String>("recipients_json").unwrap()).unwrap()
    }

    #[test]
    fn reconstruct_benign_migration_error_detection() {
        assert!(is_reconstruct_benign_migration_error(
            "table projects already exists"
        ));
        assert!(is_reconstruct_benign_migration_error(
            "duplicate column name: foo"
        ));
        assert!(is_reconstruct_benign_migration_error(
            "duplicate index name: idx_messages_created_ts"
        ));
        assert!(!is_reconstruct_benign_migration_error(
            "near \"CREATE\": syntax error"
        ));
        assert!(!is_reconstruct_benign_migration_error(
            "no such table: agents"
        ));
    }

    #[test]
    fn extract_json_frontmatter_basic() {
        let content = "---json\n{\"id\": 1, \"subject\": \"hello\"}\n---\n\nBody text here.\n";
        let fm = extract_json_frontmatter(content).expect("should extract");
        assert_eq!(fm, "{\"id\": 1, \"subject\": \"hello\"}\n");
    }

    #[test]
    fn extract_json_frontmatter_multiline() {
        let content =
            "---json\n{\n  \"id\": 42,\n  \"from\": \"TestAgent\"\n}\n---\n\nHello world.\n";
        let fm = extract_json_frontmatter(content).expect("should extract");
        assert!(fm.contains("\"id\": 42"));
        assert!(fm.contains("\"from\": \"TestAgent\""));
    }

    #[test]
    fn extract_json_frontmatter_missing() {
        assert!(extract_json_frontmatter("no frontmatter here").is_none());
        assert!(extract_json_frontmatter("---json\nno end marker").is_none());
    }

    #[test]
    fn extract_json_frontmatter_accepts_eof_after_closing_marker() {
        let content = "---json\n{\"id\": 9}\n---";
        let fm = extract_json_frontmatter(content).expect("should extract");
        assert_eq!(fm, "{\"id\": 9}\n");
        let body = extract_body_after_frontmatter(content).expect("should extract body");
        assert_eq!(body, "");
    }

    #[test]
    fn extract_body_after_frontmatter_basic() {
        let content = "---json\n{}\n---\n\nThe body content.\n";
        let body = extract_body_after_frontmatter(content).expect("should extract");
        // Trailing newline is preserved (no .trim() on body)
        assert_eq!(body, "The body content.\n");
    }

    #[test]
    fn extract_body_after_frontmatter_preserves_trailing_whitespace() {
        let content = "---json\n{}\n---\n\nLine 1\n  indented\n\nLine 3\n";
        let body = extract_body_after_frontmatter(content).expect("should extract");
        assert!(body.starts_with("Line 1\n"));
        assert!(body.ends_with("Line 3\n"));
    }

    #[test]
    fn extract_body_after_frontmatter_preserves_code_block() {
        let content =
            "---json\n{}\n---\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n";
        let body = extract_body_after_frontmatter(content).expect("should extract");
        assert!(body.starts_with("```rust\n"));
        assert!(body.ends_with("```\n"));
    }

    #[test]
    fn extract_body_after_frontmatter_strips_leading_blank_lines() {
        let content = "---json\n{}\n---\n\n\n\nBody after blanks.\n";
        let body = extract_body_after_frontmatter(content).expect("should extract");
        assert_eq!(body, "Body after blanks.\n");
    }

    #[test]
    fn extract_body_after_frontmatter_preserves_leading_spaces() {
        let content = "---json\n{}\n---\n\n    indented body\n";
        let body = extract_body_after_frontmatter(content).expect("should extract");
        assert_eq!(body, "    indented body\n");
    }

    #[test]
    fn json_str_array_variants() {
        let v: serde_json::Value = serde_json::json!({
            "to": ["Alice", " Bob ", "   "],
            "cc": " Charlie ",
            "bcc": [],
        });
        assert_eq!(json_str_array(&v, "to"), vec!["Alice", "Bob"]);
        assert_eq!(json_str_array(&v, "cc"), vec!["Charlie"]);
        assert!(json_str_array(&v, "bcc").is_empty());
        assert!(json_str_array(&v, "missing").is_empty());
    }

    #[test]
    fn normalize_reconstructed_agent_policies_coerces_invalid_values_to_auto() {
        let mut stats = ReconstructStats::default();
        assert_eq!(
            normalize_reconstructed_required_agent_field(
                Some("  claude-code  "),
                "test archive profile",
                "program",
                "unknown",
                &mut stats,
            ),
            "claude-code"
        );
        assert_eq!(
            normalize_reconstructed_required_agent_field(
                Some("   "),
                "test archive profile",
                "program",
                "unknown",
                &mut stats,
            ),
            "unknown"
        );
        assert_eq!(
            normalize_reconstructed_attachments_policy(
                Some(" INLINE "),
                "test archive profile",
                &mut stats,
            ),
            "inline"
        );
        assert_eq!(
            normalize_reconstructed_contact_policy(
                Some("\0Contacts_Only\0"),
                "test archive profile",
                &mut stats,
            ),
            "contacts_only"
        );
        assert_eq!(
            normalize_reconstructed_attachments_policy(
                Some("email"),
                "test archive profile",
                &mut stats,
            ),
            "auto"
        );
        assert_eq!(
            normalize_reconstructed_contact_policy(
                Some("contacts-only"),
                "test archive profile",
                &mut stats,
            ),
            "auto"
        );
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("test archive profile")
                && warning.contains("invalid attachments_policy")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("test archive profile") && warning.contains("empty program")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("test archive profile") && warning.contains("invalid contact_policy")
        }));
    }

    #[test]
    fn parse_salvaged_recipients_json_surfaces_malformed_payloads() {
        let mut stats = ReconstructStats::default();
        let (recipients_json, to_names, cc_names, bcc_names) =
            parse_salvaged_recipients_json(Some("{not-json".to_string()), 42, &mut stats);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recipients_json)
                .expect("recipients_json parses"),
            serde_json::json!({
                "to": [MALFORMED_RECIPIENTS_SENTINEL],
                "cc": [],
                "bcc": [],
            })
        );
        assert_eq!(to_names, vec![MALFORMED_RECIPIENTS_SENTINEL]);
        assert!(cc_names.is_empty());
        assert!(bcc_names.is_empty());
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("invalid recipients_json")
                && warning.contains("preserving malformed recipient metadata sentinel")
        }));

        let mut stats = ReconstructStats::default();
        let (_, to_names, cc_names, bcc_names) = parse_salvaged_recipients_json(
            Some(r#"{"to":[17],"cc":[],"bcc":[]}"#.to_string()),
            43,
            &mut stats,
        );
        assert_eq!(to_names, vec![MALFORMED_RECIPIENTS_SENTINEL]);
        assert!(cc_names.is_empty());
        assert!(bcc_names.is_empty());
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("non-canonical recipients_json")
                && warning.contains("preserving malformed recipient metadata sentinel")
        }));
    }

    #[test]
    fn normalize_archive_recipients_json_surfaces_malformed_payloads() {
        let mut stats = ReconstructStats::default();
        let msg = serde_json::json!({
            "to": {"name": "Bob"},
            "cc": [],
            "bcc": [],
        });
        let (recipients_json, to_names, cc_names, bcc_names) =
            normalize_archive_recipients_json(&msg, "archive/test.md", &mut stats);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recipients_json)
                .expect("recipients_json parses"),
            serde_json::json!({
                "to": [MALFORMED_RECIPIENTS_SENTINEL],
                "cc": [],
                "bcc": [],
            })
        );
        assert_eq!(to_names, vec![MALFORMED_RECIPIENTS_SENTINEL]);
        assert!(cc_names.is_empty());
        assert!(bcc_names.is_empty());
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("non-canonical recipient payload")
                && warning.contains("preserving malformed recipient metadata sentinel")
        }));

        let mut stats = ReconstructStats::default();
        let msg = serde_json::json!({
            "to": ["Bob"],
            "cc": "Carol",
            "bcc": [],
        });
        let (_, to_names, cc_names, bcc_names) =
            normalize_archive_recipients_json(&msg, "archive/test.md", &mut stats);
        assert_eq!(to_names, vec!["Bob"]);
        assert_eq!(cc_names, vec!["Carol"]);
        assert!(bcc_names.is_empty());
        assert!(stats.warnings.is_empty());
    }

    #[test]
    fn parse_salvaged_attachments_json_surfaces_malformed_payloads() {
        let mut stats = ReconstructStats::default();
        let attachments_json =
            parse_salvaged_attachments_json(Some("{not-json".to_string()), 42, &mut stats);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&attachments_json)
                .expect("attachments_json parses"),
            serde_json::json!([{
                "name": MALFORMED_ATTACHMENTS_SENTINEL,
                "media_type": serde_json::Value::Null,
                "path": serde_json::Value::Null,
                "bytes": serde_json::Value::Null,
            }])
        );
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("invalid attachments payload")
                && warning.contains("preserving malformed attachment metadata sentinel")
        }));

        let mut stats = ReconstructStats::default();
        let attachments_json = parse_salvaged_attachments_json(
            Some(r#"{"name":"artifact.txt"}"#.to_string()),
            43,
            &mut stats,
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&attachments_json)
                .expect("attachments_json parses"),
            serde_json::json!([{
                "name": MALFORMED_ATTACHMENTS_SENTINEL,
                "media_type": serde_json::Value::Null,
                "path": serde_json::Value::Null,
                "bytes": serde_json::Value::Null,
            }])
        );
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("non-array attachments payload")
                && warning.contains("preserving malformed attachment metadata sentinel")
        }));
    }

    #[test]
    fn normalized_archive_agent_name_rejects_blank_values() {
        assert_eq!(
            normalized_archive_agent_name(Some(" Alice ")),
            Some("Alice".to_string())
        );
        assert_eq!(normalized_archive_agent_name(Some("   ")), None);
        assert_eq!(normalized_archive_agent_name(None), None);
    }

    #[test]
    fn sync_reconstructed_message_recipients_json_trims_and_drops_blank_names() {
        let conn = SqliteDbConn::open_memory().expect("open in-memory db");
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL)",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, recipients_json TEXT NOT NULL DEFAULT '{}')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL, read_ts INTEGER, ack_ts INTEGER)",
        )
        .unwrap();

        conn.execute_raw("INSERT INTO messages (id, recipients_json) VALUES (1, '{}')")
            .unwrap();
        conn.execute_raw("INSERT INTO agents (id, project_id, name) VALUES (1, 1, '  Bob  ')")
            .unwrap();
        conn.execute_raw("INSERT INTO agents (id, project_id, name) VALUES (2, 1, '   ')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (1, 1, 'to')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (1, 2, 'cc')",
        )
        .unwrap();

        sync_reconstructed_message_recipients_json(&conn, 1).expect("sync recipients_json");

        assert_eq!(
            message_one_recipients_json(&conn),
            serde_json::json!({
                "to": ["Bob"],
                "cc": [],
                "bcc": [],
            })
        );
    }

    #[test]
    fn sync_reconstructed_message_recipients_json_keeps_orphaned_recipient_rows_visible() {
        let conn = SqliteDbConn::open_memory().expect("open in-memory db");
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL)",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, recipients_json TEXT NOT NULL DEFAULT '{}')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL, read_ts INTEGER, ack_ts INTEGER)",
        )
        .unwrap();

        conn.execute_raw("INSERT INTO messages (id, recipients_json) VALUES (1, '{}')")
            .unwrap();
        conn.execute_raw("INSERT INTO agents (id, project_id, name) VALUES (7, 1, 'Bob')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (1, 7, 'to')",
        )
        .unwrap();
        conn.execute_raw("DELETE FROM agents WHERE id = 7").unwrap();

        sync_reconstructed_message_recipients_json(&conn, 1).expect("sync recipients_json");

        assert_eq!(
            message_one_recipients_json(&conn),
            serde_json::json!({
                "to": ["[unknown-agent-7]"],
                "cc": [],
                "bcc": [],
            })
        );
    }

    #[test]
    fn parse_ts_iso_string() {
        let v: serde_json::Value = serde_json::json!({
            "created_ts": "2026-02-22T12:00:00Z"
        });
        let ts = parse_ts_from_json(&v, "created_ts");
        assert!(ts.is_some());
        let ts = ts.unwrap();
        // Should be in microseconds, somewhere around 2026
        assert!(ts > 1_700_000_000_000_000);
    }

    #[test]
    fn parse_ts_integer() {
        let v: serde_json::Value = serde_json::json!({
            "created_ts": 1_740_000_000_000_000_i64
        });
        let ts = parse_ts_from_json(&v, "created_ts");
        assert_eq!(ts, Some(1_740_000_000_000_000));
    }

    #[test]
    fn reconstruct_stats_display() {
        let stats = ReconstructStats {
            projects: 2,
            agents: 5,
            messages: 100,
            recipients: 200,
            duplicate_canonical_message_files: 0,
            duplicate_canonical_message_ids: 0,
            cross_project_canonical_collisions: 0,
            salvaged_projects: 0,
            salvaged_agents: 0,
            salvaged_messages: 0,
            salvaged_message_id_remaps: 0,
            salvaged_recipients: 0,
            salvaged_reservations: 0,
            salvaged_reservation_releases: 0,
            rollups_salvaged: 0,
            parse_errors: 3,
            warnings: vec![],
            duplicate_canonical_id_set: BTreeSet::new(),
        };
        let display = stats.to_string();
        assert!(display.contains("2 projects"));
        assert!(display.contains("5 agents"));
        assert!(display.contains("100 messages"));
        assert!(display.contains("3 parse errors"));
    }

    #[test]
    fn query_last_insert_or_existing_id_composite_matches_case_insensitively() {
        let conn = SqliteDbConn::open_memory().expect("open in-memory db");
        conn.execute_raw(
            "CREATE TABLE agents (\
                id INTEGER PRIMARY KEY,\
                project_id INTEGER NOT NULL,\
                name TEXT NOT NULL\
            )",
        )
        .expect("create agents table");
        conn.query_sync(
            "INSERT INTO agents (project_id, name) VALUES (1, 'BlueLake')",
            &[],
        )
        .expect("insert agent");

        let id = query_last_insert_or_existing_id_composite(
            &conn,
            "agents",
            "project_id",
            1,
            "name",
            "bluelake",
        )
        .expect("find agent id case-insensitively");

        assert_eq!(id, 1);
    }

    #[test]
    fn reconstruct_empty_storage_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 0);
        assert_eq!(stats.agents, 0);
        assert_eq!(stats.messages, 0);
    }

    #[test]
    fn reconstruct_empty_projects_directory_does_not_create_database() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects")).unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 0);
        assert_eq!(stats.agents, 0);
        assert_eq!(stats.messages, 0);
        assert!(
            stats
                .warnings
                .iter()
                .any(|warning| warning.contains("No project archives found")),
            "empty projects dir should be reported as empty archive content: {:?}",
            stats.warnings
        );
        assert!(
            !db_path.exists(),
            "empty archive reconstruct should not create a database file"
        );
    }

    #[test]
    fn reconstruct_refuses_an_existing_target_without_mutating_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("existing.sqlite3");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects").join("demo")).unwrap();

        let existing = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        existing
            .execute_raw("CREATE TABLE sentinel (value TEXT NOT NULL); INSERT INTO sentinel VALUES ('original')")
            .unwrap();
        drop(existing);
        crate::pool::wal_checkpoint_truncate_path(&db_path).unwrap();

        let error = reconstruct_from_archive(&db_path, &storage_root)
            .expect_err("low-level reconstruct must never reuse a live/partial target");
        assert!(
            error.to_string().contains("fresh candidate path"),
            "unexpected error: {error}"
        );
        let existing = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = existing
            .query_sync("SELECT value FROM sentinel", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<String>("value").unwrap(), "original");
    }

    #[test]
    fn reconstruct_candidate_does_not_touch_live_sibling_atc_sidecar() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let candidate = tmp.path().join("candidate.sqlite3");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects").join("demo")).unwrap();
        let atc_sidecar = tmp.path().join("atc.sqlite3");
        let sentinel = b"live-atc-sidecar-must-remain-byte-identical";
        std::fs::write(&atc_sidecar, sentinel).unwrap();

        reconstruct_from_archive(&candidate, &storage_root)
            .expect("fresh candidate reconstruction should succeed");

        assert_eq!(
            std::fs::read(&atc_sidecar).unwrap(),
            sentinel,
            "candidate construction must never open, migrate, quarantine, or replace the fixed-name live ATC sidecar"
        );
    }

    #[test]
    fn reconstruct_with_agent_profile() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        // Create fake archive structure
        let project_dir = storage_root.join("projects").join("test-project");
        let agent_dir = project_dir.join("agents").join("TestAgent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        let profile = serde_json::json!({
            "name": "TestAgent",
            "program": "claude-code",
            "model": "opus-4.6",
            "task_description": "testing",
            "inception_ts": "2026-02-22T12:00:00Z",
            "last_active_ts": "2026-02-22T12:00:00Z",
            "attachments_policy": "auto",
        });
        std::fs::write(
            agent_dir.join("profile.json"),
            serde_json::to_string_pretty(&profile).unwrap(),
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 1);
        assert_eq!(stats.agents, 1);
        assert_eq!(stats.messages, 0);
        assert_eq!(stats.parse_errors, 0);
        assert!(
            crate::pool::sqlite_file_is_healthy(&db_path)
                .expect("canonical sqlite health check should succeed"),
            "reconstructed database should be healthy for canonical sqlite",
        );
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open rebuilt db");
        // ATC telemetry now lives in the dedicated sidecar DB (atc.sqlite3),
        // which is independent of the Git archive and untouched by reconstruct
        // (br-bvq1x.11.7). The rebuilt primary mailbox DB must therefore contain
        // NO atc_* tables.
        // `_` in LIKE matches the literal underscore here (no ESCAPE needed);
        // there are no non-`atc_` tables that would be falsely matched, and the
        // assertion only cares that the set is empty.
        let atc_tables = conn
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND name LIKE 'atc_%' \
                 ORDER BY name",
                &[],
            )
            .expect("query ATC tables")
            .into_iter()
            .filter_map(|row| row.get_named::<String>("name").ok())
            .collect::<Vec<_>>();
        assert!(
            atc_tables.is_empty(),
            "reconstruct must NOT materialize atc_* tables in the primary mailbox DB \
             (ATC telemetry is isolated in the atc.sqlite3 sidecar); found: {atc_tables:?}"
        );
    }

    #[test]
    fn reconstruct_with_agent_profile_normalizes_invalid_policy_values_to_auto() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test_invalid_agent_policy.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        let agent_dir = project_dir.join("agents").join("TestAgent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        let profile = serde_json::json!({
            "name": "TestAgent",
            "program": "   ",
            "model": "\t",
            "inception_ts": "2026-02-22T12:00:00Z",
            "last_active_ts": "2026-02-22T12:00:00Z",
            "attachments_policy": "email",
            "contact_policy": "contacts-only",
        });
        std::fs::write(
            agent_dir.join("profile.json"),
            serde_json::to_string_pretty(&profile).unwrap(),
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("archive agent profile") && warning.contains("empty program")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("archive agent profile") && warning.contains("empty model")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("archive agent profile")
                && warning.contains("invalid attachments_policy")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("archive agent profile") && warning.contains("invalid contact_policy")
        }));

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open rebuilt db");
        let agent_rows = conn
            .query_sync(
                "SELECT program, model, attachments_policy, contact_policy
                 FROM agents
                 WHERE name = 'TestAgent'",
                &[],
            )
            .expect("query agent");
        assert_eq!(agent_rows.len(), 1);
        assert_eq!(
            agent_rows[0]
                .get_named::<String>("program")
                .expect("program"),
            "unknown"
        );
        assert_eq!(
            agent_rows[0].get_named::<String>("model").expect("model"),
            "unknown"
        );
        assert_eq!(
            agent_rows[0]
                .get_named::<String>("attachments_policy")
                .expect("attachments_policy"),
            "auto"
        );
        assert_eq!(
            agent_rows[0]
                .get_named::<String>("contact_policy")
                .expect("contact_policy"),
            "auto"
        );
    }

    #[test]
    fn reconstruct_trims_archive_agent_directory_names_before_matching_messages() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test_trimmed_archive_agent_name.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        let agent_dir = project_dir.join("agents").join(" Alice ");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&messages_dir).unwrap();

        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{
                "name":"Alice",
                "program":"claude-code",
                "model":"opus-4.6",
                "inception_ts":"2026-02-22T12:00:00Z",
                "last_active_ts":"2026-02-22T12:00:00Z"
            }"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__hello__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "Hello",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

hello
"#,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("non-canonical name") && warning.contains("\" Alice \"")
        }));

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open rebuilt db");
        let agent_rows = conn
            .query_sync("SELECT name, program FROM agents ORDER BY name", &[])
            .expect("query agents");
        assert_eq!(
            agent_rows.len(),
            2,
            "Alice profile plus Bob recipient placeholder"
        );
        assert_eq!(
            agent_rows[0]
                .get_named::<String>("name")
                .expect("first name"),
            "Alice"
        );
        assert_eq!(
            agent_rows[0]
                .get_named::<String>("program")
                .expect("Alice program"),
            "claude-code"
        );
        assert_eq!(
            agent_rows[1]
                .get_named::<String>("name")
                .expect("second name"),
            "Bob"
        );
    }

    #[test]
    fn reconstruct_prefers_profile_name_when_archive_agent_directory_mismatches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test_profile_name_mismatch.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        let agent_dir = project_dir.join("agents").join("LegacyAlice");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&messages_dir).unwrap();

        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{
                "name":"Alice",
                "program":"claude-code",
                "model":"opus-4.6",
                "inception_ts":"2026-02-22T12:00:00Z",
                "last_active_ts":"2026-02-22T12:00:00Z"
            }"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__hello__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "Hello",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

hello
"#,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("disagrees with directory name")
                && warning.contains("\"LegacyAlice\"")
                && warning.contains("\"Alice\"")
        }));

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open rebuilt db");
        let agent_rows = conn
            .query_sync("SELECT name, program FROM agents ORDER BY name", &[])
            .expect("query agents");
        assert_eq!(
            agent_rows.len(),
            2,
            "Alice profile plus Bob recipient placeholder"
        );
        assert_eq!(
            agent_rows[0]
                .get_named::<String>("name")
                .expect("first name"),
            "Alice"
        );
        assert_eq!(
            agent_rows[0]
                .get_named::<String>("program")
                .expect("Alice program"),
            "claude-code"
        );
        assert_eq!(
            agent_rows[1]
                .get_named::<String>("name")
                .expect("second name"),
            "Bob"
        );
    }

    #[test]
    fn scan_archive_message_inventory_counts_projects_and_agents_without_messages() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let storage_root = tmp.path().join("storage");
        let alpha_agent = storage_root
            .join("projects")
            .join("alpha")
            .join("agents")
            .join("Alice");
        let beta_dir = storage_root.join("projects").join("beta");
        let beta_agent = beta_dir.join("agents").join("Bob");
        let beta_messages = beta_dir.join("messages").join("2026").join("04");
        std::fs::create_dir_all(&alpha_agent).expect("create alpha agent dir");
        std::fs::create_dir_all(&beta_agent).expect("create beta agent dir");
        std::fs::create_dir_all(&beta_messages).expect("create beta messages dir");
        std::fs::write(alpha_agent.join("profile.json"), "{}").expect("write alpha profile");
        std::fs::write(beta_agent.join("profile.json"), "{}").expect("write beta profile");
        std::fs::write(
            beta_messages.join("2026-04-01T12-00-00Z__hello__7.md"),
            r#"---json
{
  "id": 7,
  "from": "Bob",
  "to": ["Alice"],
  "subject": "Hello",
  "importance": "normal",
  "created_ts": "2026-04-01T12:00:00Z"
}
---

body
"#,
        )
        .expect("write canonical message");

        let inventory = scan_archive_message_inventory(&storage_root);
        assert_eq!(inventory.projects, 2);
        assert_eq!(inventory.agents, 2);
        assert_eq!(inventory.unique_message_ids, 1);
        assert_eq!(inventory.latest_message_id, Some(7));
        assert!(
            inventory.project_identities.contains(
                &MailboxProjectIdentity::from_parts(Some("alpha".to_string()), None, None,)
                    .expect("alpha identity")
            )
        );
        assert!(
            inventory.project_identities.contains(
                &MailboxProjectIdentity::from_parts(Some("beta".to_string()), None, None,)
                    .expect("beta identity")
            )
        );
    }

    #[test]
    fn archive_missing_project_identities_detects_same_count_wrong_project() {
        let archive = ArchiveMessageInventory {
            projects: 1,
            agents: 1,
            project_identities: std::iter::once(
                MailboxProjectIdentity::from_parts(
                    Some("archive-project".to_string()),
                    Some("/archive-project".to_string()),
                    None,
                )
                .expect("archive identity"),
            )
            .collect(),
            ..ArchiveMessageInventory::default()
        };
        let db_identities = std::iter::once(
            MailboxProjectIdentity::from_parts(
                Some("wrong-project".to_string()),
                Some("/wrong-project".to_string()),
                None,
            )
            .expect("db identity"),
        )
        .collect();

        let missing = archive_missing_project_identities(&archive, &db_identities);
        assert_eq!(missing, vec!["archive-project (/archive-project)"]);
    }

    #[test]
    fn archive_missing_project_identities_detects_same_slug_different_human_key() {
        let archive = ArchiveMessageInventory {
            projects: 1,
            agents: 1,
            project_identities: std::iter::once(
                MailboxProjectIdentity::from_parts(
                    Some("shared-slug".to_string()),
                    Some("/archive-project".to_string()),
                    None,
                )
                .expect("archive identity"),
            )
            .collect(),
            ..ArchiveMessageInventory::default()
        };
        let db_identities = std::iter::once(
            MailboxProjectIdentity::from_parts(
                Some("shared-slug".to_string()),
                Some("/wrong-project".to_string()),
                None,
            )
            .expect("db identity"),
        )
        .collect();

        let missing = archive_missing_project_identities(&archive, &db_identities);
        assert_eq!(missing, vec!["shared-slug (/archive-project)"]);
    }

    #[cfg(unix)]
    #[test]
    fn reconstruct_skips_symlinked_project_directories() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");
        let real_project = tmp.path().join("outside-project");
        let real_agent = real_project.join("agents").join("Ghost");
        let real_messages = real_project.join("messages").join("2026").join("03");
        let linked_project = storage_root.join("projects").join("linked-project");

        std::fs::create_dir_all(&real_agent).unwrap();
        std::fs::create_dir_all(&real_messages).unwrap();
        std::fs::create_dir_all(linked_project.parent().unwrap()).unwrap();
        std::fs::write(real_agent.join("profile.json"), "{}").unwrap();
        std::fs::write(
            real_messages.join("note.md"),
            "---json\n{\"from\":\"Ghost\",\"to\":[],\"subject\":\"hi\"}\n---\nbody\n",
        )
        .unwrap();
        symlink(&real_project, &linked_project).unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 0);
        assert_eq!(stats.agents, 0);
        assert_eq!(stats.messages, 0);
    }

    #[cfg(unix)]
    #[test]
    fn reconstruct_warns_on_symlinked_storage_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let real_storage = tmp.path().join("real-storage");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(real_storage.join("projects")).unwrap();
        symlink(&real_storage, &storage_root).unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 0);
        assert_eq!(stats.agents, 0);
        assert_eq!(stats.messages, 0);
        assert!(
            !db_path.exists(),
            "symlinked storage roots should not create a reconstructed database file"
        );
        assert!(
            stats
                .warnings
                .iter()
                .any(|warning| warning.contains("not a real directory")),
            "expected symlinked storage root warning, got {:?}",
            stats.warnings
        );
    }

    #[cfg(unix)]
    #[test]
    fn reconstruct_rejects_symlinked_destination_path() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let real_db = tmp.path().join("real.db");
        let linked_db = tmp.path().join("linked.db");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects")).unwrap();
        symlink(&real_db, &linked_db).unwrap();

        let err = reconstruct_from_archive(&linked_db, &storage_root)
            .expect_err("symlinked reconstruct destinations must be rejected");
        assert!(
            err.to_string().contains("symlinked path"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reconstruct_rejects_symlinked_destination_parent() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let real_parent = tmp.path().join("real-parent");
        let linked_parent = tmp.path().join("linked-parent");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&real_parent).unwrap();
        std::fs::create_dir_all(storage_root.join("projects")).unwrap();
        symlink(&real_parent, &linked_parent).unwrap();
        let db_path = linked_parent.join("test.db");

        let err = reconstruct_from_archive(&db_path, &storage_root)
            .expect_err("symlinked reconstruct destination parents must be rejected");
        assert!(
            err.to_string().contains("symlinked path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn reconstruct_uses_project_metadata_human_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let metadata = serde_json::json!({
            "slug": "test-project",
            "human_key": "/data/projects/exact-human-key",
        });
        std::fs::write(
            project_dir.join("project.json"),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 1);

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT slug, human_key FROM projects WHERE slug = 'test-project'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let human_key = rows[0]
            .get_named::<String>("human_key")
            .expect("human_key text");
        assert_eq!(human_key, "/data/projects/exact-human-key");
    }

    #[test]
    fn reconstruct_falls_back_when_project_metadata_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 1);
        assert!(
            stats
                .warnings
                .iter()
                .any(|w| w.contains("Missing") && w.contains("project.json"))
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT human_key FROM projects WHERE slug = 'test-project'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let human_key = rows[0]
            .get_named::<String>("human_key")
            .expect("human_key text");
        assert_eq!(human_key, "/test-project");
    }

    #[test]
    fn reconstruct_with_salvage_upgrades_slug_only_archive_project_placeholder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed.db");
        let salvage_db_path = tmp.path().join("salvage.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    human_key TEXT,
                    created_at INTEGER
                )",
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (100, 'test-project', '/test-project', 1)",
                &[],
            )
            .unwrap();

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("salvage merge should succeed");
        assert_eq!(stats.projects, 1);
        assert_eq!(stats.salvaged_projects, 0);

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT id, slug, human_key, created_at FROM projects ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<i64>("id").unwrap(),
            1_i64,
            "archive placeholder project id should remain stable"
        );
        assert_eq!(rows[0].get_named::<String>("slug").unwrap(), "test-project");
        assert_eq!(
            rows[0].get_named::<String>("human_key").unwrap(),
            "/test-project"
        );
        assert_eq!(
            rows[0].get_named::<i64>("created_at").unwrap(),
            1_i64,
            "salvage database should promote project created_at"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reconstruct_with_salvage_fails_closed_for_symlinked_salvage_parent() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");
        let real_parent = tmp.path().join("real-salvage");
        let linked_parent = tmp.path().join("linked-salvage");
        std::fs::create_dir_all(storage_root.join("projects")).unwrap();
        std::fs::create_dir_all(&real_parent).unwrap();
        symlink(&real_parent, &linked_parent).unwrap();

        let real_salvage_db_path = real_parent.join("salvage.db");
        let salvage_db_path = linked_parent.join("salvage.db");
        let salvage_conn = SqliteDbConn::open_file(real_salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw("CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL)")
            .unwrap();
        drop(salvage_conn);

        let error =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect_err("a rejected salvage path must block archive-only reconstruction");
        assert!(
            error.to_string().contains("symlinked path")
                && error
                    .to_string()
                    .contains("refusing an archive-only candidate"),
            "expected a fail-closed symlink error, got {error}"
        );
        assert!(
            !db_path.exists(),
            "a rejected salvage path must not create a promotable candidate"
        );
    }

    #[test]
    fn reconstruct_with_salvage_keeps_same_basename_projects_and_children_distinct() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed.db");
        let salvage_db_path = tmp.path().join("salvage.db");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects").join("shared")).unwrap();

        let salvage = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage
            .execute_raw(&schema::init_schema_sql_base())
            .unwrap();
        // Deliberately collide the source numeric project id with the archive
        // candidate's first project id. Stable identity, never row id or
        // basename, must decide ownership of every salvaged child.
        salvage
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES \
                     (1, 'srv-team-shared', '/srv/team/shared', 1); \
                 INSERT INTO agents (id, project_id, name) VALUES (7, 1, 'CanonicalAgent');",
            )
            .unwrap();
        drop(salvage);

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("stable-key salvage should preserve both repositories");
        assert_eq!(stats.salvaged_projects, 1);

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();

        let rows = conn
            .query_sync(
                "SELECT id, slug, human_key, created_at FROM projects ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get_named::<i64>("id").unwrap(), 1);
        assert_eq!(
            rows[0].get_named::<String>("slug").unwrap(),
            "shared".to_string()
        );
        assert_eq!(
            rows[0].get_named::<String>("human_key").unwrap(),
            "/shared".to_string()
        );
        assert!(rows[0].get_named::<i64>("created_at").unwrap() > 0);
        assert_eq!(rows[1].get_named::<i64>("id").unwrap(), 2);
        assert_eq!(
            rows[1].get_named::<String>("slug").unwrap(),
            "srv-team-shared".to_string()
        );
        assert_eq!(
            rows[1].get_named::<String>("human_key").unwrap(),
            "/srv/team/shared".to_string()
        );
        assert_eq!(rows[1].get_named::<i64>("created_at").unwrap(), 1);

        let agent_rows = conn
            .query_sync(
                "SELECT p.slug AS project_slug, p.human_key AS project_human_key \
                 FROM agents AS a JOIN projects AS p ON p.id = a.project_id \
                 WHERE a.name = 'CanonicalAgent'",
                &[],
            )
            .unwrap();
        assert_eq!(agent_rows.len(), 1);
        assert_eq!(
            agent_rows[0].get_named::<String>("project_slug").unwrap(),
            "srv-team-shared"
        );
        assert_eq!(
            agent_rows[0]
                .get_named::<String>("project_human_key")
                .unwrap(),
            "/srv/team/shared"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reconstruct_with_message() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        // Create fake archive structure
        let project_dir = storage_root.join("projects").join("test-project");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&messages_dir).unwrap();

        // Create agent profile
        let agent_dir = project_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"test","model":"test","inception_ts":"2026-02-22T12:00:00Z","last_active_ts":"2026-02-22T12:00:00Z"}"#,
        )
        .unwrap();

        // Create message file
        let msg_content = r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "cc": [],
  "bcc": ["Carol"],
  "thread_id": "TEST-1",
  "subject": "Hello Bob",
  "importance": "normal",
  "ack_required": false,
  "created_ts": "2026-02-22T12:00:00Z",
  "attachments": []
}
---

Hello Bob, this is a test message.
"#;
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__hello-bob__1.md"),
            msg_content,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 1);
        assert_eq!(
            stats.agents, 1,
            "Alice from profile; Bob and Carol auto-created as placeholders (not counted in stats)"
        );
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.recipients, 2);
        assert_eq!(stats.parse_errors, 0);

        // Verify the message was inserted correctly
        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT subject, body_md, thread_id, recipients_json FROM messages LIMIT 1",
                &[],
            )
            .unwrap();
        assert!(!rows.is_empty(), "message should exist in DB");
        let recipients_json = rows[0]
            .get_named::<String>("recipients_json")
            .expect("recipients_json");
        let recipients_value: serde_json::Value =
            serde_json::from_str(&recipients_json).expect("recipients_json parses");
        assert_eq!(recipients_value["to"], serde_json::json!(["Bob"]));
        assert_eq!(recipients_value["cc"], serde_json::json!([]));
        assert_eq!(recipients_value["bcc"], serde_json::json!(["Carol"]));

        // Verify Bob was auto-created as a placeholder agent
        let agent_rows = conn
            .query_sync("SELECT name, program FROM agents ORDER BY name", &[])
            .unwrap();
        assert_eq!(
            agent_rows.len(),
            3,
            "Alice, Bob, and Carol should all exist"
        );
        // Verify Alice has the correct program from profile
        let alice_rows = conn
            .query_sync("SELECT program FROM agents WHERE name = 'Alice'", &[])
            .unwrap();
        assert!(!alice_rows.is_empty());
        // Verify Bob was auto-created with 'unknown' program
        let bob_rows = conn
            .query_sync("SELECT program FROM agents WHERE name = 'Bob'", &[])
            .unwrap();
        assert!(!bob_rows.is_empty());
        let carol_rows = conn
            .query_sync("SELECT program FROM agents WHERE name = 'Carol'", &[])
            .unwrap();
        assert!(!carol_rows.is_empty());

        let recipient_rows = conn
            .query_sync(
                "SELECT a.name AS name, mr.kind AS kind
                 FROM message_recipients mr
                 JOIN agents a ON a.id = mr.agent_id
                 ORDER BY mr.kind, a.name",
                &[],
            )
            .unwrap();
        assert_eq!(recipient_rows.len(), 2);
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("kind")
                .expect("first recipient kind"),
            "bcc"
        );
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("name")
                .expect("first recipient name"),
            "Carol"
        );
        assert_eq!(
            recipient_rows[1]
                .get_named::<String>("kind")
                .expect("second recipient kind"),
            "to"
        );
        assert_eq!(
            recipient_rows[1]
                .get_named::<String>("name")
                .expect("second recipient name"),
            "Bob"
        );
    }

    #[test]
    fn reconstruct_preserves_nontrivial_canonical_message_id() {
        // br-bvq1x.7.5 (G5) golden, before/after: a single archived message
        // carrying a non-trivial canonical id (904) must land in a *fresh*
        // (empty) DB under that exact id. Under AUTOINCREMENT the first
        // inserted row would otherwise be re-keyed to 1, so asserting the row
        // id == 904 cleanly distinguishes canonical-identity preservation
        // (`INSERT OR REPLACE ... (id, ...)`) from SQLite reassigning the id.
        // `reconstruct_with_message` only exercises id 1, which is ambiguous
        // (autoincrement would also pick 1); this is the dedicated regression
        // guard for the preservation path.
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&messages_dir).unwrap();
        let agent_dir = project_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"test","model":"test","inception_ts":"2026-02-22T12:00:00Z","last_active_ts":"2026-02-22T12:00:00Z"}"#,
        )
        .unwrap();

        let msg_content = r#"---json
{
  "id": 904,
  "from": "Alice",
  "to": ["Bob"],
  "thread_id": "TEST-904",
  "subject": "Canonical id golden",
  "importance": "normal",
  "ack_required": false,
  "created_ts": "2026-02-22T12:00:00Z",
  "attachments": []
}
---

Body for the canonical id golden test.
"#;
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__canonical-id-golden__904.md"),
            msg_content,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.parse_errors, 0);

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT id, subject FROM messages", &[])
            .unwrap();
        assert_eq!(rows.len(), 1, "exactly one message should be reconstructed");
        assert_eq!(
            rows[0].get_named::<i64>("id").expect("message id"),
            904,
            "reconstruct must preserve the canonical message id, not re-key it via autoincrement"
        );
        assert_eq!(
            rows[0]
                .get_named::<String>("subject")
                .expect("message subject"),
            "Canonical id golden"
        );
    }

    #[test]
    fn reconstruct_handles_malformed_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"test-project","human_key":"/test-project","created_at":0}"#,
        )
        .unwrap();

        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&messages_dir).unwrap();

        // Malformed file (no frontmatter)
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__bad__1.md"),
            "This file has no frontmatter at all.",
        )
        .unwrap();

        // Another malformed file (invalid JSON)
        std::fs::write(
            messages_dir.join("2026-02-22T12-01-00Z__bad__2.md"),
            "---json\n{invalid json}\n---\n\nBody.\n",
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.messages, 0);
        assert_eq!(stats.parse_errors, 2, "both bad files should be counted");
        assert_eq!(stats.warnings.len(), 2);
    }

    #[test]
    fn reconstruct_from_archive_surfaces_malformed_attachment_payloads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"test-project","human_key":"/test-project","created_at":0}"#,
        )
        .unwrap();

        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__bad-attachments__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "Bad attachments",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z",
  "attachments": {"name":"artifact.txt"}
}
---

Body.
"#,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.messages, 1);
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("non-array attachments payload")
                && warning.contains("preserving malformed attachment metadata sentinel")
        }));

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT attachments FROM messages WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let attachments_json = rows[0]
            .get_named::<String>("attachments")
            .expect("attachments");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&attachments_json)
                .expect("attachments parses"),
            serde_json::json!([{
                "name": MALFORMED_ATTACHMENTS_SENTINEL,
                "media_type": serde_json::Value::Null,
                "path": serde_json::Value::Null,
                "bytes": serde_json::Value::Null,
            }])
        );
    }

    #[test]
    fn reconstruct_from_archive_surfaces_malformed_recipient_payloads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"test-project","human_key":"/test-project","created_at":0}"#,
        )
        .unwrap();

        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__bad-recipients__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob", 17],
  "cc": [],
  "bcc": [],
  "subject": "Bad recipients",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

Body.
"#,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.recipients, 1);
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("non-canonical recipient payload")
                && warning.contains("preserving malformed recipient metadata sentinel")
        }));

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT recipients_json FROM messages WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let recipients_json = rows[0]
            .get_named::<String>("recipients_json")
            .expect("recipients_json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recipients_json)
                .expect("recipients_json parses"),
            serde_json::json!({
                "to": [MALFORMED_RECIPIENTS_SENTINEL],
                "cc": [],
                "bcc": [],
            })
        );

        let recipient_rows = conn
            .query_sync(
                "SELECT a.name AS name, mr.kind AS kind
                 FROM message_recipients mr
                 JOIN agents a ON a.id = mr.agent_id
                 WHERE mr.message_id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(recipient_rows.len(), 1);
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("kind")
                .expect("recipient kind"),
            "to"
        );
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("name")
                .expect("recipient name"),
            MALFORMED_RECIPIENTS_SENTINEL
        );
    }

    #[test]
    fn reconstruct_skips_duplicate_canonical_message_id_without_merging_recipients() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("dup-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"dup-project","human_key":"/dup-project","created_at":0}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-02-22T00:00:00Z"}"#,
        )
        .unwrap();

        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__first__7.md"),
            r#"---json
{
  "id": 7,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

first body
"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-01-00Z__second__7.md"),
            r#"---json
{
  "id": 7,
  "from": "Alice",
  "to": ["Carol"],
  "subject": "Second copy",
  "importance": "urgent",
  "created_ts": "2026-02-22T12:01:00Z"
}
---

second body
"#,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.messages, 1, "duplicate canonical id must be skipped");
        assert_eq!(stats.duplicate_canonical_message_files, 1);
        assert_eq!(stats.duplicate_canonical_message_ids, 1);
        assert_eq!(
            stats.recipients, 1,
            "duplicate recipient rows must not merge"
        );
        assert!(
            stats
                .warnings
                .iter()
                .any(|warning| warning.contains("Duplicate canonical message id 7")),
            "expected duplicate-id warning, got {:?}",
            stats.warnings
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let subject_rows = conn
            .query_sync("SELECT subject FROM messages WHERE id = 7", &[])
            .unwrap();
        assert_eq!(subject_rows.len(), 1);
        assert_eq!(
            subject_rows[0]
                .get_named::<String>("subject")
                .expect("subject"),
            "First copy"
        );

        let recipient_rows = conn
            .query_sync(
                "SELECT a.name AS name \
                 FROM message_recipients mr \
                 JOIN agents a ON a.id = mr.agent_id \
                 WHERE mr.message_id = 7 \
                 ORDER BY a.name",
                &[],
            )
            .unwrap();
        assert_eq!(recipient_rows.len(), 1);
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("name")
                .expect("recipient name"),
            "Bob"
        );
    }

    #[test]
    fn reconstruct_preserves_cross_project_canonical_id_collision_under_generated_db_id() {
        // Two separate project archives both contain a message with frontmatter
        // id 7. Prior behavior dropped the second as a duplicate. Expected
        // behavior: both messages are preserved, the second inserted under an
        // auto-generated DB id, with a cross-project collision warning.
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        for (slug, file_slug, subject_body, sender, recipient) in [
            ("project-a", "alice-a", "Alice A", "Alice", "Bob"),
            ("project-b", "alice-b", "Alice B", "Alice", "Carol"),
        ] {
            let project_dir = storage_root.join("projects").join(slug);
            let agent_dir = project_dir.join("agents").join(sender);
            let messages_dir = project_dir.join("messages").join("2026").join("02");
            std::fs::create_dir_all(&agent_dir).unwrap();
            std::fs::create_dir_all(&messages_dir).unwrap();
            std::fs::write(
                project_dir.join("project.json"),
                format!(r#"{{"slug":"{slug}","human_key":"/{slug}","created_at":0}}"#),
            )
            .unwrap();
            std::fs::write(
                agent_dir.join("profile.json"),
                format!(
                    r#"{{"agent_name":"{sender}","program":"coder","model":"test","registered_ts":"2026-02-22T00:00:00Z"}}"#,
                ),
            )
            .unwrap();
            std::fs::write(
                messages_dir.join(format!("2026-02-22T12-00-00Z__{file_slug}__7.md")),
                format!(
                    r#"---json
{{
  "id": 7,
  "from": "{sender}",
  "to": ["{recipient}"],
  "subject": "{subject_body}",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}}
---

body for {slug}
"#
                ),
            )
            .unwrap();
        }

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(
            stats.messages, 2,
            "both messages must be preserved across projects"
        );
        assert_eq!(
            stats.duplicate_canonical_message_files, 0,
            "cross-project collisions must not count as duplicates"
        );
        assert_eq!(stats.cross_project_canonical_collisions, 1);
        assert!(
            stats
                .warnings
                .iter()
                .any(|w| w.contains("Cross-project canonical message id 7")),
            "expected cross-project warning, got {:?}",
            stats.warnings
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let subject_rows = conn
            .query_sync("SELECT subject FROM messages ORDER BY subject", &[])
            .unwrap();
        assert_eq!(subject_rows.len(), 2, "both messages must exist in DB");
        let subjects: Vec<String> = subject_rows
            .iter()
            .map(|r| r.get_named::<String>("subject").expect("subject"))
            .collect();
        assert_eq!(subjects, vec!["Alice A".to_string(), "Alice B".to_string()]);

        // Exactly one message keeps canonical id 7; the other is re-keyed.
        let canonical_rows = conn
            .query_sync("SELECT id FROM messages WHERE id = 7", &[])
            .unwrap();
        assert_eq!(canonical_rows.len(), 1);

        // Both messages must keep their original project association — the
        // collision recovery must not collapse them into a single project.
        let project_pair_rows = conn
            .query_sync("SELECT COUNT(DISTINCT project_id) AS n FROM messages", &[])
            .unwrap();
        assert_eq!(project_pair_rows.len(), 1);
        assert_eq!(
            project_pair_rows[0].get_named::<i64>("n").unwrap(),
            2,
            "messages must remain attached to their original distinct projects"
        );
    }

    #[test]
    fn finalize_cross_project_canonical_collision_warnings_emits_summary_above_sample_limit() {
        // Below or at the sample limit: no summary line — the per-collision
        // warnings already itemize everything.
        let mut at_limit = ReconstructStats {
            cross_project_canonical_collisions: DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT,
            ..ReconstructStats::default()
        };
        at_limit.finalize_cross_project_canonical_collision_warnings();
        assert!(
            at_limit.warnings.is_empty(),
            "no summary expected at the sample limit, got {:?}",
            at_limit.warnings
        );

        // Above the sample limit: emit a single summary so the diagnostic
        // count survives even when the per-occurrence warning loop stopped.
        let mut over_limit = ReconstructStats {
            cross_project_canonical_collisions: DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT + 7,
            ..ReconstructStats::default()
        };
        over_limit.finalize_cross_project_canonical_collision_warnings();
        assert_eq!(
            over_limit.warnings.len(),
            1,
            "exactly one summary line expected above the sample limit"
        );
        let summary = &over_limit.warnings[0];
        let expected_collision_count = (DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT + 7).to_string();
        assert!(
            summary.contains(&expected_collision_count),
            "summary must report the total collision count, got: {summary}"
        );
        assert!(
            summary.contains("cross-project"),
            "summary must mention cross-project, got: {summary}"
        );
    }

    #[test]
    fn reconstruct_sanitizes_invalid_thread_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("thread-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"thread-project","human_key":"/thread-project","created_at":0}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-02-22T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__thread__9.md"),
            r#"---json
{
  "id": 9,
  "from": "Alice",
  "to": ["Bob"],
  "thread_id": "  !!br:123??  ",
  "subject": "Thread sanitize",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

thread body
"#,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert!(
            stats
                .warnings
                .iter()
                .any(|warning| warning.contains("Sanitized invalid thread_id")),
            "expected thread-id warning, got {:?}",
            stats.warnings
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT thread_id FROM messages WHERE id = 9", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<String>("thread_id").expect("thread_id"),
            "br123"
        );
    }

    #[test]
    fn reconstruct_trims_sender_and_recipient_names() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("trim-project");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"trim-project","human_key":"/trim-project","created_at":0}"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__trim__1.md"),
            r#"---json
{
  "id": 1,
  "from": "   ",
  "to": [" Bob ", "   "],
  "cc": " Carol ",
  "subject": "Trim names",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

body
"#,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.recipients, 2);

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let agent_rows = conn
            .query_sync("SELECT name FROM agents ORDER BY name", &[])
            .unwrap();
        let names: Vec<String> = agent_rows
            .iter()
            .map(|row| row.get_named::<String>("name").expect("name"))
            .collect();
        assert_eq!(names, vec!["Bob", "Carol", "unknown"]);

        let sender_rows = conn
            .query_sync(
                "SELECT a.name AS name \
                 FROM messages m JOIN agents a ON a.id = m.sender_id \
                 WHERE m.id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(
            sender_rows[0].get_named::<String>("name").expect("sender"),
            "unknown"
        );
    }

    #[test]
    fn reconstruct_recovers_file_reservations_from_archive() {
        let storage_root = tempfile::tempdir().expect("tempdir");
        let db_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = storage_root
            .path()
            .join("projects")
            .join("reservation-project");
        let agents_dir = project_dir.join("agents").join("CoralMarsh");
        let reservations_dir = project_dir.join("file_reservations");
        std::fs::create_dir_all(&agents_dir).expect("create agents dir");
        std::fs::create_dir_all(&reservations_dir).expect("create reservations dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"reservation-project","human_key":"/reservation-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agents_dir.join("profile.json"),
            r#"{
                "name": "CoralMarsh",
                "program": "codex-cli",
                "model": "gpt-5",
                "task_description": "reservation snapshot",
                "inception_ts": "2026-03-13T21:21:02Z",
                "last_active_ts": "2026-03-13T21:21:02Z"
            }"#,
        )
        .expect("write agent profile");
        let reservation_json = r#"{
            "id": 904,
            "project": "/reservation-project",
            "agent": "CoralMarsh",
            "path_pattern": "crates/mcp-agent-mail-cli/src/robot.rs",
            "exclusive": true,
            "reason": "br-q0e0u",
            "created_ts": "2026-03-13T21:36:47.221175Z",
            "expires_ts": "2026-03-13T23:36:47.221175Z"
        }"#;
        std::fs::write(reservations_dir.join("id-904.json"), reservation_json)
            .expect("write canonical reservation artifact");
        std::fs::write(
            reservations_dir.join("bb1d1d9f8a400a6c3e5732b41fc1f253986e4077.json"),
            reservation_json,
        )
        .expect("write mirrored reservation artifact");
        std::fs::write(
            reservations_dir.join("id-905.json"),
            r#"{
                "id": 905,
                "project": "/reservation-project",
                "agent": "BlueLake",
                "path": "crates/mcp-agent-mail-db/src/reconstruct.rs",
                "exclusive": false,
                "reason": "python-compat",
                "created_ts": "2026-03-13T21:40:00Z",
                "expires_ts": "2026-03-13T23:40:00Z"
            }"#,
        )
        .expect("write python-format reservation artifact");

        let db_path = db_dir.path().join("reconstruct_reservations.sqlite3");
        reconstruct_from_archive(&db_path, storage_root.path()).expect("reconstruct");

        let conn = SqliteDbConn::open_file(db_path.display().to_string()).expect("open db");
        let rows = conn
            .query_sync(
                "SELECT fr.id, a.name AS agent_name, fr.path_pattern, fr.exclusive, fr.reason
                 FROM file_reservations fr
                 JOIN agents a ON a.id = fr.agent_id
                 ORDER BY fr.id ASC",
                &[],
            )
            .expect("query reservations");

        assert_eq!(rows.len(), 2, "reconstruction should recover both formats");
        assert_eq!(rows[0].get_named::<i64>("id").unwrap(), 904);
        assert_eq!(
            rows[0].get_named::<String>("agent_name").unwrap(),
            "CoralMarsh"
        );
        assert_eq!(
            rows[0].get_named::<String>("path_pattern").unwrap(),
            "crates/mcp-agent-mail-cli/src/robot.rs"
        );
        assert_eq!(rows[0].get_named::<i64>("exclusive").unwrap(), 1);
        assert_eq!(rows[0].get_named::<String>("reason").unwrap(), "br-q0e0u");
        assert_eq!(rows[1].get_named::<i64>("id").unwrap(), 905);
        assert_eq!(
            rows[1].get_named::<String>("agent_name").unwrap(),
            "BlueLake"
        );
        assert_eq!(
            rows[1].get_named::<String>("path_pattern").unwrap(),
            "crates/mcp-agent-mail-db/src/reconstruct.rs"
        );
        assert_eq!(rows[1].get_named::<i64>("exclusive").unwrap(), 0);
        assert_eq!(
            rows[1].get_named::<String>("reason").unwrap(),
            "python-compat"
        );
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn reconstruct_with_salvage_merges_db_only_rows_and_recipient_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed.db");
        let salvage_db_path = tmp.path().join("salvage.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"test-project","human_key":"/test-project","created_at":0}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-02-22T00:00:00Z","last_active_ts":"2026-02-22T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__archive__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "Archive copy",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

archive body
"#,
        )
        .unwrap();

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    human_key TEXT,
                    created_at INTEGER
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agents (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    name TEXT NOT NULL
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    sender_id INTEGER NOT NULL,
                    subject TEXT,
                    body_md TEXT,
                    created_ts INTEGER
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE message_recipients (
                    message_id INTEGER NOT NULL,
                    agent_id INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    read_ts INTEGER,
                    ack_ts INTEGER
                )",
            )
            .unwrap();

        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (100, 'test-project', '/test-project', 1)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agents (id, project_id, name) VALUES
                    (10, 100, 'Alice'),
                    (11, 100, 'Bob'),
                    (12, 100, 'Carol')",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts)
                 VALUES (2, 100, 10, 'DB-only', 'db body', 2)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts)
                 VALUES
                    (1, 11, 'TO ', 123, 456),
                    (2, 12, 'to', NULL, NULL)",
                &[],
            )
            .unwrap();

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("salvage merge should succeed");
        assert_eq!(stats.projects, 1);
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.salvaged_projects, 0);
        assert_eq!(stats.salvaged_agents, 1);
        assert_eq!(stats.salvaged_messages, 1);
        assert_eq!(stats.salvaged_recipients, 2);

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let message_rows = conn
            .query_sync(
                "SELECT id, subject, recipients_json FROM messages ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(message_rows.len(), 2);
        assert_eq!(
            message_rows[1]
                .get_named::<String>("subject")
                .expect("subject"),
            "DB-only"
        );
        let db_only_recipients_json = message_rows[1]
            .get_named::<String>("recipients_json")
            .expect("db-only recipients_json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&db_only_recipients_json)
                .expect("db-only recipients_json parses"),
            serde_json::json!({
                "to": ["Carol"],
                "cc": [],
                "bcc": [],
            })
        );

        let recipient_rows = conn
            .query_sync(
                "SELECT a.name AS name, mr.read_ts AS read_ts, mr.ack_ts AS ack_ts
                 FROM message_recipients mr
                 JOIN agents a ON a.id = mr.agent_id
                 WHERE mr.message_id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(recipient_rows.len(), 1);
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("name")
                .expect("recipient name"),
            "Bob"
        );
        assert_eq!(
            recipient_rows[0]
                .get_named::<i64>("read_ts")
                .expect("read_ts"),
            123
        );
        assert_eq!(
            recipient_rows[0]
                .get_named::<i64>("ack_ts")
                .expect("ack_ts"),
            456
        );

        let carol_rows = conn
            .query_sync(
                "SELECT a.name AS name
                 FROM message_recipients mr
                 JOIN agents a ON a.id = mr.agent_id
                 WHERE mr.message_id = 2",
                &[],
            )
            .unwrap();
        assert_eq!(carol_rows.len(), 1);
        assert_eq!(
            carol_rows[0]
                .get_named::<String>("name")
                .expect("recipient name"),
            "Carol"
        );
    }

    #[test]
    fn reconstruct_with_salvage_remaps_cross_project_message_id_and_recipient_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_message_collision.db");
        let salvage_db_path = tmp.path().join("salvage_message_collision.db");
        let storage_root = tmp.path().join("storage");

        let archive_project = storage_root.join("projects").join("archive-project");
        let archive_agent = archive_project.join("agents").join("Alice");
        let archive_messages = archive_project.join("messages").join("2026").join("07");
        std::fs::create_dir_all(&archive_agent).expect("create archive agent");
        std::fs::create_dir_all(&archive_messages).expect("create archive messages");
        std::fs::write(
            archive_project.join("project.json"),
            r#"{"slug":"archive-project","human_key":"/archive-project","created_at":1}"#,
        )
        .expect("write archive project");
        std::fs::write(
            archive_agent.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":1,"last_active_ts":2}"#,
        )
        .expect("write archive agent");
        std::fs::write(
            archive_messages.join("2026-07-17T12-00-00Z__archive__7.md"),
            r#"---json
{"id":7,"from":"Alice","to":[],"subject":"Archive message","importance":"normal","created_ts":"2026-07-17T12:00:00Z","attachments":[]}
---

archive body
"#,
        )
        .expect("write archive message");

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(&crate::schema::init_schema_sql_base())
            .expect("init salvage schema");
        salvage_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES
                    (500, 'db-project', '/db-project', 1);
                 INSERT INTO agents
                    (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                    VALUES
                    (600, 500, 'Bob', 'coder', 'test', '', 1, 2, 'auto', 'auto'),
                    (601, 500, 'Carol', 'coder', 'test', '', 1, 2, 'auto', 'auto');
                 INSERT INTO messages
                    (id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments)
                    VALUES
                    (7, 500, 600, 'DB-only message', 'db body', 'urgent', 1, 3,
                     '{\"to\":[\"Carol\"],\"cc\":[],\"bcc\":[]}', '[]');
                 INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts)
                    VALUES (7, 601, 'to', 4, 5);",
            )
            .expect("seed colliding DB-only message");

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("cross-project numeric message collision should be remapped");
        assert_eq!(stats.salvaged_messages, 1);
        assert_eq!(stats.salvaged_message_id_remaps, 1);

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let message_rows = conn
            .query_sync(
                "SELECT m.id, p.slug, a.name AS sender, m.subject
                 FROM messages m
                 JOIN projects p ON p.id = m.project_id
                 JOIN agents a ON a.id = m.sender_id
                 ORDER BY p.slug",
                &[],
            )
            .expect("query reconstructed messages");
        assert_eq!(message_rows.len(), 2);
        let db_row = message_rows
            .iter()
            .find(|row| row.get_named::<String>("slug").ok().as_deref() == Some("db-project"))
            .expect("DB-only message survived");
        assert_ne!(db_row.get_named::<i64>("id").unwrap(), 7);
        assert_eq!(db_row.get_named::<String>("sender").unwrap(), "Bob");
        assert_eq!(
            db_row.get_named::<String>("subject").unwrap(),
            "DB-only message"
        );

        let recipient_rows = conn
            .query_sync(
                "SELECT mp.slug AS message_project, ap.slug AS agent_project,
                        a.name, mr.read_ts, mr.ack_ts
                 FROM message_recipients mr
                 JOIN messages m ON m.id = mr.message_id
                 JOIN projects mp ON mp.id = m.project_id
                 JOIN agents a ON a.id = mr.agent_id
                 JOIN projects ap ON ap.id = a.project_id
                 WHERE m.subject = 'DB-only message'",
                &[],
            )
            .expect("query remapped recipient state");
        assert_eq!(recipient_rows.len(), 1);
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("message_project")
                .unwrap(),
            "db-project"
        );
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("agent_project")
                .unwrap(),
            "db-project"
        );
        assert_eq!(
            recipient_rows[0].get_named::<String>("name").unwrap(),
            "Carol"
        );
        assert_eq!(recipient_rows[0].get_named::<i64>("read_ts").unwrap(), 4);
        assert_eq!(recipient_rows[0].get_named::<i64>("ack_ts").unwrap(), 5);
    }

    #[test]
    fn reconstruct_with_salvage_preserves_active_reservations_and_release_ledger() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_reservations.db");
        let salvage_db_path = tmp.path().join("salvage_reservations.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&agent_dir).expect("create archive agent");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"test-project","human_key":"/test-project","created_at":1}"#,
        )
        .expect("write archive project");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":1,"last_active_ts":2}"#,
        )
        .expect("write archive agent");

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(&crate::schema::init_schema_sql_base())
            .expect("init salvage schema");
        salvage_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at)
                    VALUES (100, 'test-project', '/test-project', 1);
                 INSERT INTO agents
                    (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                    VALUES (200, 100, 'Alice', 'coder', 'test', '', 1, 2, 'auto', 'auto');
                 INSERT INTO file_reservations
                    (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts)
                    VALUES
                    (900, 100, 200, 'src/active/**', 1, 'active work', 10, 1000, NULL),
                    (901, 100, 200, 'src/released/**', 0, 'finished work', 20, 2000, NULL);
                 INSERT INTO file_reservation_releases (reservation_id, released_ts)
                    VALUES (901, 250);",
            )
            .expect("seed reservation continuity state");

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("reservation continuity should be salvaged through stable identities");
        assert_eq!(stats.salvaged_reservations, 2);
        assert_eq!(stats.salvaged_reservation_releases, 1);

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT p.slug, a.name, fr.path_pattern, fr.exclusive, fr.reason,
                        fr.created_ts, fr.expires_ts,
                        COALESCE(rr.released_ts, fr.released_ts) AS effective_released_ts
                 FROM file_reservations fr
                 JOIN projects p ON p.id = fr.project_id
                 JOIN agents a ON a.id = fr.agent_id
                 LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id
                 ORDER BY fr.path_pattern",
                &[],
            )
            .expect("query salvaged reservations");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get_named::<String>("slug").unwrap(), "test-project");
        assert_eq!(rows[0].get_named::<String>("name").unwrap(), "Alice");
        assert_eq!(
            rows[0].get_named::<String>("path_pattern").unwrap(),
            "src/active/**"
        );
        assert_eq!(rows[0].get_named::<i64>("exclusive").unwrap(), 1);
        assert_eq!(
            rows[0].get_named::<String>("reason").unwrap(),
            "active work"
        );
        assert_eq!(rows[0].get_named::<i64>("created_ts").unwrap(), 10);
        assert_eq!(rows[0].get_named::<i64>("expires_ts").unwrap(), 1000);
        assert!(rows[0].get_named::<i64>("effective_released_ts").is_err());
        assert_eq!(
            rows[1].get_named::<i64>("effective_released_ts").unwrap(),
            250
        );
    }

    #[test]
    fn reconstruct_with_salvage_rolls_back_ambiguous_reservation_ownership() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_reservation_collision.db");
        let salvage_db_path = tmp.path().join("salvage_reservation_collision.db");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects")).expect("archive root");

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(&crate::schema::init_schema_sql_base())
            .expect("init salvage schema");
        salvage_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at)
                    VALUES (100, 'db-only-project', '/db-only-project', 1);
                 INSERT INTO agents
                    (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                    VALUES (200, 100, 'Alice', 'coder', 'test', '', 1, 2, 'auto', 'auto');
                 INSERT INTO file_reservations
                    (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts)
                    VALUES
                    (900, 100, 200, 'src/**', 1, 'same key', 10, 1000, NULL),
                    (901, 100, 200, 'src/**', 0, 'same key', 10, 1000, NULL);",
            )
            .expect("seed ambiguous reservation ownership");

        let error =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect_err("ambiguous stable reservation ownership must fail closed");
        assert!(
            error
                .to_string()
                .contains("conflicts with target reservation")
                && error.to_string().contains("exclusive ownership")
                && error
                    .to_string()
                    .contains("refusing to promote the archive-only candidate"),
            "unexpected fail-closed error: {error}"
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let project_rows = conn
            .query_sync(
                "SELECT COUNT(*) AS count FROM projects WHERE slug = 'db-only-project'",
                &[],
            )
            .expect("query rollback state");
        assert_eq!(project_rows[0].get_named::<i64>("count").unwrap(), 0);
        let reservation_rows = conn
            .query_sync("SELECT COUNT(*) AS count FROM file_reservations", &[])
            .expect("query rolled-back reservations");
        assert_eq!(reservation_rows[0].get_named::<i64>("count").unwrap(), 0);
    }

    #[test]
    fn reconstruct_with_salvage_preserves_agent_links_and_product_bus_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_contacts_products.db");
        let salvage_db_path = tmp.path().join("salvage_contacts_products.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        let alice_dir = project_dir.join("agents").join("Alice");
        let bob_dir = project_dir.join("agents").join("Bob");
        std::fs::create_dir_all(&alice_dir).expect("create alice dir");
        std::fs::create_dir_all(&bob_dir).expect("create bob dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"test-project","human_key":"/test-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            alice_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-02-22T00:00:00Z","last_active_ts":"2026-02-22T00:00:00Z"}"#,
        )
        .expect("write alice profile");
        std::fs::write(
            bob_dir.join("profile.json"),
            r#"{"name":"Bob","program":"coder","model":"test","inception_ts":"2026-02-22T00:00:00Z","last_active_ts":"2026-02-22T00:00:00Z"}"#,
        )
        .expect("write bob profile");

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(&crate::schema::init_schema_sql_base())
            .expect("init salvage schema");
        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (100, 'test-project', '/test-project', 1)",
                &[],
            )
            .expect("insert salvage project");
        salvage_conn
            .query_sync(
                "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES
                    (10, 100, 'Alice', 'coder', 'test', '', 1, 2, 'auto', 'auto'),
                    (11, 100, 'Bob', 'coder', 'test', '', 1, 2, 'auto', 'auto')",
                &[],
            )
            .expect("insert salvage agents");
        salvage_conn
            .query_sync(
                "INSERT INTO agent_links (a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts, expires_ts)
                 VALUES (100, 10, 100, 11, 'approved', 'carry contact state', 7, 8, 9)",
                &[],
            )
            .expect("insert agent link");
        salvage_conn
            .query_sync(
                "INSERT INTO products (id, product_uid, name, created_at) VALUES (700, 'prod-test', 'Test Product', 10)",
                &[],
            )
            .expect("insert product");
        salvage_conn
            .query_sync(
                "INSERT INTO product_project_links (product_id, project_id, created_at) VALUES (700, 100, 11)",
                &[],
            )
            .expect("insert product link");

        reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
            .expect("salvage merge should preserve db-only rows");

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let contact_rows = conn
            .query_sync(
                "SELECT status, reason FROM agent_links ORDER BY id ASC",
                &[],
            )
            .expect("query agent_links");
        assert_eq!(contact_rows.len(), 1);
        assert_eq!(
            contact_rows[0]
                .get_named::<String>("status")
                .expect("status"),
            "approved"
        );
        assert_eq!(
            contact_rows[0]
                .get_named::<String>("reason")
                .expect("reason"),
            "carry contact state"
        );

        let product_rows = conn
            .query_sync(
                "SELECT p.product_uid, p.name, pr.slug
                 FROM products p
                 JOIN product_project_links ppl ON ppl.product_id = p.id
                 JOIN projects pr ON pr.id = ppl.project_id",
                &[],
            )
            .expect("query product bus rows");
        assert_eq!(product_rows.len(), 1);
        assert_eq!(
            product_rows[0]
                .get_named::<String>("product_uid")
                .expect("product uid"),
            "prod-test"
        );
        assert_eq!(
            product_rows[0].get_named::<String>("name").expect("name"),
            "Test Product"
        );
        assert_eq!(
            product_rows[0].get_named::<String>("slug").expect("slug"),
            "test-project"
        );
    }

    #[test]
    fn reconstruct_with_salvage_rolls_back_partial_merge_on_late_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_salvage_rollback.db");
        let salvage_db_path = tmp.path().join("salvage_rollback.db");
        let storage_root = tmp.path().join("storage");

        std::fs::create_dir_all(storage_root.join("projects")).unwrap();

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(&crate::schema::init_schema_sql_base())
            .expect("init salvage schema");
        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (100, 'rollback-project', '/rollback-project', 1)",
                &[],
            )
            .expect("insert salvage project");
        salvage_conn
            .query_sync(
                "INSERT INTO agents
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                 VALUES (10, 100, 'Alice', 'coder', 'test', '', 1, 2, 'auto', 'auto')",
                &[],
            )
            .expect("insert salvage agent");

        FAIL_SALVAGE_MERGE_AFTER_PROJECTS.store(true, std::sync::atomic::Ordering::SeqCst);
        let error =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect_err("forced late salvage failure must block candidate promotion");
        assert!(
            error
                .to_string()
                .contains("reconstruct salvage: forced failure after projects")
                && error
                    .to_string()
                    .contains("refusing to promote the archive-only candidate"),
            "error should include the merge failure and fail-closed invariant: {error}"
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let project_rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM projects", &[])
            .expect("query project count");
        let project_count: i64 = project_rows[0].get_named("cnt").expect("project count");
        assert_eq!(
            project_count, 0,
            "failed salvage merge should not leak partially inserted projects"
        );

        let agent_rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM agents", &[])
            .expect("query agent count");
        let agent_count: i64 = agent_rows[0].get_named("cnt").expect("agent count");
        assert_eq!(
            agent_count, 0,
            "failed salvage merge should not leak partially inserted agents"
        );
    }

    #[test]
    fn reconstruct_with_salvage_fails_closed_when_message_query_is_corrupt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_corrupt_salvage.db");
        let salvage_db_path = tmp.path().join("salvage_corrupt_message_scan.db");
        let storage_root = tmp.path().join("storage");

        std::fs::create_dir_all(storage_root.join("projects")).unwrap();

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    human_key TEXT,
                    created_at INTEGER
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agents (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    name TEXT NOT NULL
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    sender_id INTEGER NOT NULL,
                    subject TEXT,
                    body_md TEXT,
                    created_ts INTEGER
                )",
            )
            .unwrap();

        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (100, 'corrupt-source-project', '/corrupt-source-project', 1)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agents (id, project_id, name)
                 VALUES (10, 100, 'Alice')",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts)
                 VALUES (2, 100, 10, 'DB-only', 'db body', 2)",
                &[],
            )
            .unwrap();

        FAIL_SALVAGE_QUERY_MESSAGES.store(true, std::sync::atomic::Ordering::SeqCst);
        let error =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect_err("corrupt salvage source must block candidate promotion");

        assert!(
            error.to_string().contains(
                "reconstruct salvage: query messages: Query error: database disk image is malformed"
            ),
            "error should include corrupt message query failure: {error}"
        );
        assert!(
            error
                .to_string()
                .contains("refusing to promote the archive-only candidate"),
            "error should explain the fail-closed continuity invariant: {error}"
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let message_rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM messages", &[])
            .expect("query message count");
        let message_count: i64 = message_rows[0].get_named("cnt").expect("message count");
        assert_eq!(
            message_count, 0,
            "failed salvage transaction must not leak DB-only messages"
        );
    }

    #[test]
    fn reconstruct_with_salvage_fails_closed_when_supplied_path_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_missing_salvage.db");
        let salvage_db_path = tmp.path().join("missing-salvage.db");
        let storage_root = tmp.path().join("archive");
        std::fs::create_dir(&storage_root).expect("archive root");

        let error =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect_err("a supplied missing salvage path must block candidate promotion");

        assert!(
            error
                .to_string()
                .contains("refusing an archive-only candidate"),
            "error should explain the fail-closed continuity invariant: {error}"
        );
        assert!(
            !db_path.exists(),
            "a failed salvage probe must not create a promotable candidate"
        );
    }

    #[test]
    fn reconstruct_with_salvage_rebuilds_recipients_when_recipient_table_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_missing_recipients.db");
        let salvage_db_path = tmp.path().join("salvage_missing_recipients.db");
        let storage_root = tmp.path().join("storage");

        std::fs::create_dir_all(storage_root.join("projects")).unwrap();

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    human_key TEXT,
                    created_at INTEGER
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agents (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    name TEXT NOT NULL
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    sender_id INTEGER NOT NULL,
                    subject TEXT,
                    body_md TEXT,
                    created_ts INTEGER,
                    recipients_json TEXT
                )",
            )
            .unwrap();

        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (100, 'test-project', '/test-project', 1)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agents (id, project_id, name) VALUES
                    (10, 100, 'Alice'),
                    (11, 100, 'Bob'),
                    (12, 100, 'Carol')",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts, recipients_json)
                 VALUES
                    (2, 100, 10, 'DB-only', 'db body', 2, '{\"to\":[\"Bob\"],\"cc\":\"Carol\",\"bcc\":[]}')",
                &[],
            )
            .unwrap();

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("salvage merge should succeed");
        assert_eq!(stats.salvaged_projects, 1);
        assert_eq!(stats.salvaged_agents, 3);
        assert_eq!(stats.salvaged_messages, 1);
        assert_eq!(stats.salvaged_recipients, 2);

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let message_rows = conn
            .query_sync("SELECT recipients_json FROM messages WHERE id = 2", &[])
            .unwrap();
        assert_eq!(message_rows.len(), 1);
        let recipients_json = message_rows[0]
            .get_named::<String>("recipients_json")
            .expect("recipients_json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recipients_json)
                .expect("recipients_json parses"),
            serde_json::json!({
                "to": ["Bob"],
                "cc": ["Carol"],
                "bcc": [],
            })
        );

        let recipient_rows = conn
            .query_sync(
                "SELECT a.name AS name, mr.kind AS kind
                 FROM message_recipients mr
                 JOIN agents a ON a.id = mr.agent_id
                 WHERE mr.message_id = 2
                 ORDER BY mr.kind, a.name",
                &[],
            )
            .unwrap();
        assert_eq!(recipient_rows.len(), 2);
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("kind")
                .expect("first recipient kind"),
            "cc"
        );
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("name")
                .expect("first recipient name"),
            "Carol"
        );
        assert_eq!(
            recipient_rows[1]
                .get_named::<String>("kind")
                .expect("second recipient kind"),
            "to"
        );
        assert_eq!(
            recipient_rows[1]
                .get_named::<String>("name")
                .expect("second recipient name"),
            "Bob"
        );
    }

    #[test]
    fn reconstruct_with_salvage_surfaces_malformed_recipients_json_instead_of_dropping_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_malformed_recipients.db");
        let salvage_db_path = tmp.path().join("salvage_malformed_recipients.db");
        let storage_root = tmp.path().join("storage");

        std::fs::create_dir_all(storage_root.join("projects")).unwrap();

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    human_key TEXT,
                    created_at INTEGER
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agents (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    name TEXT NOT NULL
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    sender_id INTEGER NOT NULL,
                    subject TEXT,
                    body_md TEXT,
                    created_ts INTEGER,
                    recipients_json TEXT
                )",
            )
            .unwrap();

        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (100, 'test-project', '/test-project', 1)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agents (id, project_id, name) VALUES (10, 100, 'Alice')",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts, recipients_json)
                 VALUES (2, 100, 10, 'DB-only', 'db body', 2, '{not-json')",
                &[],
            )
            .unwrap();

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("salvage merge should succeed");
        assert_eq!(stats.salvaged_messages, 1);
        assert_eq!(stats.salvaged_recipients, 1);
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("invalid recipients_json")
                && warning.contains("preserving malformed recipient metadata sentinel")
        }));

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let message_rows = conn
            .query_sync("SELECT recipients_json FROM messages WHERE id = 2", &[])
            .unwrap();
        assert_eq!(message_rows.len(), 1);
        let recipients_json = message_rows[0]
            .get_named::<String>("recipients_json")
            .expect("recipients_json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recipients_json)
                .expect("recipients_json parses"),
            serde_json::json!({
                "to": [MALFORMED_RECIPIENTS_SENTINEL],
                "cc": [],
                "bcc": [],
            })
        );

        let recipient_rows = conn
            .query_sync(
                "SELECT a.name AS name, mr.kind AS kind
                 FROM message_recipients mr
                 JOIN agents a ON a.id = mr.agent_id
                 WHERE mr.message_id = 2",
                &[],
            )
            .unwrap();
        assert_eq!(recipient_rows.len(), 1);
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("kind")
                .expect("recipient kind"),
            "to"
        );
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("name")
                .expect("recipient name"),
            MALFORMED_RECIPIENTS_SENTINEL
        );
    }

    #[test]
    fn reconstruct_with_salvage_surfaces_malformed_attachments_instead_of_preserving_invalid_payload()
     {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_malformed_attachments.db");
        let salvage_db_path = tmp.path().join("salvage_malformed_attachments.db");
        let storage_root = tmp.path().join("storage");

        std::fs::create_dir_all(storage_root.join("projects")).unwrap();

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    human_key TEXT,
                    created_at INTEGER
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agents (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    name TEXT NOT NULL
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    sender_id INTEGER NOT NULL,
                    subject TEXT,
                    body_md TEXT,
                    created_ts INTEGER,
                    attachments TEXT
                )",
            )
            .unwrap();

        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (100, 'test-project', '/test-project', 1)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agents (id, project_id, name) VALUES (10, 100, 'Alice')",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts, attachments)
                 VALUES (2, 100, 10, 'DB-only', 'db body', 2, '{\"name\":\"artifact.txt\"}')",
                &[],
            )
            .unwrap();

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("salvage merge should succeed");
        assert_eq!(stats.salvaged_messages, 1);
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("non-array attachments payload")
                && warning.contains("preserving malformed attachment metadata sentinel")
        }));

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT attachments FROM messages WHERE id = 2", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let attachments_json = rows[0]
            .get_named::<String>("attachments")
            .expect("attachments");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&attachments_json)
                .expect("attachments parses"),
            serde_json::json!([{
                "name": MALFORMED_ATTACHMENTS_SENTINEL,
                "media_type": serde_json::Value::Null,
                "path": serde_json::Value::Null,
                "bytes": serde_json::Value::Null,
            }])
        );
    }

    #[test]
    fn reconstruct_with_salvage_enriches_fallback_project_and_agent_metadata() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_enriched.db");
        let salvage_db_path = tmp.path().join("salvage_enriched.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("orphan-slug");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__archive__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "Archive copy",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

archive body
"#,
        )
        .unwrap();

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    human_key TEXT,
                    created_at INTEGER
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agents (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    name TEXT NOT NULL,
                    program TEXT,
                    model TEXT,
                    task_description TEXT,
                    inception_ts INTEGER,
                    last_active_ts INTEGER,
                    attachments_policy TEXT,
                    contact_policy TEXT
                )",
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (100, 'orphan-slug', '/Users/demo/projects/orphan', 123)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agents
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                 VALUES
                    (10, 100, 'Alice', 'codex-cli', 'gpt-5', 'investigating', 10, 99, 'inline', 'contacts_only'),
                    (11, 100, 'Bob', 'claude-code', 'sonnet', 'reviewing', 20, 120, 'auto', 'open')",
                &[],
            )
            .unwrap();

        reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
            .expect("salvage merge should enrich fallback rows");

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let project_rows = conn
            .query_sync(
                "SELECT human_key, created_at FROM projects WHERE slug = 'orphan-slug'",
                &[],
            )
            .unwrap();
        assert_eq!(project_rows.len(), 1);
        assert_eq!(
            project_rows[0]
                .get_named::<String>("human_key")
                .expect("human_key"),
            "/Users/demo/projects/orphan"
        );
        assert_eq!(
            project_rows[0]
                .get_named::<i64>("created_at")
                .expect("created_at"),
            123
        );

        let alice_rows = conn
            .query_sync(
                "SELECT program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy
                 FROM agents
                 WHERE name = 'Alice'",
                &[],
            )
            .unwrap();
        assert_eq!(alice_rows.len(), 1);
        let alice = &alice_rows[0];
        assert_eq!(alice.get_named::<String>("program").unwrap(), "codex-cli");
        assert_eq!(alice.get_named::<String>("model").unwrap(), "gpt-5");
        assert_eq!(
            alice.get_named::<String>("task_description").unwrap(),
            "investigating"
        );
        assert_eq!(alice.get_named::<i64>("inception_ts").unwrap(), 10);
        assert_eq!(alice.get_named::<i64>("last_active_ts").unwrap(), 99);
        assert_eq!(
            alice.get_named::<String>("attachments_policy").unwrap(),
            "inline"
        );
        assert_eq!(
            alice.get_named::<String>("contact_policy").unwrap(),
            "contacts_only"
        );

        let bob_rows = conn
            .query_sync(
                "SELECT program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy
                 FROM agents
                 WHERE name = 'Bob'",
                &[],
            )
            .unwrap();
        assert_eq!(bob_rows.len(), 1);
        let bob = &bob_rows[0];
        assert_eq!(bob.get_named::<String>("program").unwrap(), "claude-code");
        assert_eq!(bob.get_named::<String>("model").unwrap(), "sonnet");
        assert_eq!(
            bob.get_named::<String>("task_description").unwrap(),
            "reviewing"
        );
        assert_eq!(bob.get_named::<i64>("inception_ts").unwrap(), 20);
        assert_eq!(bob.get_named::<i64>("last_active_ts").unwrap(), 120);
        assert_eq!(
            bob.get_named::<String>("attachments_policy").unwrap(),
            "auto"
        );
        assert_eq!(bob.get_named::<String>("contact_policy").unwrap(), "open");
    }

    #[test]
    fn reconstruct_with_salvage_normalizes_agent_policy_values() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_policy_normalized.db");
        let salvage_db_path = tmp.path().join("salvage_policy_normalized.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        let bob_dir = project_dir.join("agents").join("Bob");
        std::fs::create_dir_all(&bob_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"test-project","human_key":"/test-project","created_at":1}"#,
        )
        .unwrap();
        std::fs::write(
            bob_dir.join("profile.json"),
            r#"{
                "name":"Bob",
                "program":"   ",
                "model":"\t",
                "inception_ts":"2026-02-22T00:00:00Z",
                "last_active_ts":"2026-02-22T00:00:00Z",
                "attachments_policy":"email",
                "contact_policy":"contacts-only"
            }"#,
        )
        .unwrap();

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    human_key TEXT,
                    created_at INTEGER
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agents (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    name TEXT NOT NULL,
                    program TEXT,
                    model TEXT,
                    task_description TEXT,
                    inception_ts INTEGER,
                    last_active_ts INTEGER,
                    attachments_policy TEXT,
                    contact_policy TEXT
                )",
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (100, 'test-project', '/test-project', 1)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agents
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                 VALUES
                    (10, 100, 'Bob', 'salvage-program', 'salvage-model', 'salvaged bob', 10, 99, ' INLINE ', ' Contacts_Only '),
                    (11, 100, 'Alice', '   ', '\t', 'salvaged alice', 11, 100, 'email', 'reject'),
                    (12, 100, 'Carol', 'salvage-program', 'salvage-model', 'salvaged carol', 12, 101, ' FILE ', ' OPEN ')",
                &[],
            )
            .unwrap();

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("salvage merge should normalize agent policies");
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("archive agent profile") && warning.contains("empty program")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("archive agent profile") && warning.contains("empty model")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("archive agent profile")
                && warning.contains("invalid attachments_policy")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("archive agent profile") && warning.contains("invalid contact_policy")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("salvage agent row 11 (Alice)") && warning.contains("empty program")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("salvage agent row 11 (Alice)") && warning.contains("empty model")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("salvage agent row 11 (Alice)")
                && warning.contains("invalid attachments_policy")
        }));
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("salvage agent row 11 (Alice)")
                && warning.contains("invalid contact_policy")
        }));

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let agent_rows = conn
            .query_sync(
                "SELECT name, program, model, attachments_policy, contact_policy
                 FROM agents
                 ORDER BY name",
                &[],
            )
            .unwrap();
        assert_eq!(agent_rows.len(), 3);

        let alice = &agent_rows[0];
        assert_eq!(alice.get_named::<String>("name").unwrap(), "Alice");
        assert_eq!(alice.get_named::<String>("program").unwrap(), "unknown");
        assert_eq!(alice.get_named::<String>("model").unwrap(), "unknown");
        assert_eq!(
            alice.get_named::<String>("attachments_policy").unwrap(),
            "auto"
        );
        assert_eq!(alice.get_named::<String>("contact_policy").unwrap(), "auto");

        let bob = &agent_rows[1];
        assert_eq!(bob.get_named::<String>("name").unwrap(), "Bob");
        assert_eq!(
            bob.get_named::<String>("program").unwrap(),
            "salvage-program"
        );
        assert_eq!(bob.get_named::<String>("model").unwrap(), "salvage-model");
        assert_eq!(
            bob.get_named::<String>("attachments_policy").unwrap(),
            "inline"
        );
        assert_eq!(
            bob.get_named::<String>("contact_policy").unwrap(),
            "contacts_only"
        );

        let carol = &agent_rows[2];
        assert_eq!(carol.get_named::<String>("name").unwrap(), "Carol");
        assert_eq!(
            carol.get_named::<String>("program").unwrap(),
            "salvage-program"
        );
        assert_eq!(carol.get_named::<String>("model").unwrap(), "salvage-model");
        assert_eq!(
            carol.get_named::<String>("attachments_policy").unwrap(),
            "file"
        );
        assert_eq!(carol.get_named::<String>("contact_policy").unwrap(), "open");
    }

    // ========================================================================
    // Archive drift report tests
    // ========================================================================

    fn write_archive_message(storage_root: &Path, slug: &str, id: i64) {
        let messages_dir = storage_root
            .join("projects")
            .join(slug)
            .join("messages")
            .join("2026")
            .join("03");
        std::fs::create_dir_all(&messages_dir).unwrap();
        let filename = format!("2026-03-01T00-00-00Z__test__{id}.md");
        std::fs::write(
            messages_dir.join(filename),
            format!(
                "---json\n{{\"id\": {id}, \"from\": \"Alice\", \"to\": [\"Bob\"], \"subject\": \"msg {id}\", \"importance\": \"normal\", \"created_ts\": 1709251200000000}}\n---\n\nBody {id}\n"
            ),
        )
        .unwrap();
    }

    fn setup_db_with_messages(db_path: &Path, ids: &[i64]) {
        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_raw(
            "CREATE TABLE IF NOT EXISTS projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL UNIQUE,
                human_key TEXT,
                created_at INTEGER
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE IF NOT EXISTS agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT,
                model TEXT,
                task_description TEXT,
                inception_ts INTEGER,
                last_active_ts INTEGER,
                attachments_policy TEXT,
                contact_policy TEXT
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                sender_id INTEGER NOT NULL,
                thread_id TEXT,
                subject TEXT,
                body_md TEXT,
                importance TEXT,
                ack_required INTEGER DEFAULT 0,
                created_ts INTEGER,
                recipients_json TEXT,
                attachments TEXT DEFAULT '[]'
            )",
        )
        .unwrap();
        conn.query_sync(
            "INSERT OR IGNORE INTO projects (id, slug, human_key, created_at) VALUES (1, 'test-project', '/test/project', 100)",
            &[],
        )
        .unwrap();
        conn.query_sync(
            "INSERT OR IGNORE INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
             VALUES (1, 1, 'Alice', 'test', 'test', '', 100, 100, 'auto', 'auto')",
            &[],
        )
        .unwrap();
        for &id in ids {
            conn.query_sync(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, importance, created_ts, recipients_json) \
                 VALUES (?, 1, 1, 'test', 'body', 'normal', 100, '{}')",
                &[Value::BigInt(id)],
            )
            .unwrap();
        }
    }

    #[test]
    fn scan_archive_message_ids_finds_all_positive_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let storage_root = tmp.path().join("storage");
        write_archive_message(&storage_root, "proj-a", 10);
        write_archive_message(&storage_root, "proj-a", 20);
        write_archive_message(&storage_root, "proj-b", 30);

        let (ids, errors) = scan_archive_message_ids(&storage_root);
        assert_eq!(errors, 0);
        assert_eq!(ids, BTreeSet::from([10, 20, 30]));
    }

    #[test]
    fn scan_archive_message_ids_empty_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (ids, errors) = scan_archive_message_ids(tmp.path());
        assert_eq!(errors, 0);
        assert!(ids.is_empty());
    }

    #[test]
    fn collect_db_message_ids_returns_all_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        setup_db_with_messages(&db_path, &[5, 15, 25]);
        let ids = collect_db_message_ids(&db_path).unwrap();
        assert_eq!(ids, BTreeSet::from([5, 15, 25]));
    }

    #[test]
    fn collect_db_message_ids_missing_table() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("empty.db");
        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_raw("CREATE TABLE dummy (id INTEGER)").unwrap();
        drop(conn);
        let ids = collect_db_message_ids(&db_path).unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn collect_db_message_ids_rejects_memory_db() {
        let err = collect_db_message_ids(Path::new(":memory:"))
            .expect_err("in-memory message-id inventory should be unavailable");
        assert!(
            err.to_string().contains("in-memory"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn collect_db_message_ids_rejects_symlinked_db_path() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real_db = tmp.path().join("real.db");
        let linked_db = tmp.path().join("linked.db");
        setup_db_with_messages(&real_db, &[5, 15, 25]);
        symlink(&real_db, &linked_db).unwrap();

        let err = collect_db_message_ids(&linked_db)
            .expect_err("DB inventory should not follow symlinked sqlite paths");
        assert!(
            err.to_string().contains("symlinked path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn drift_report_aligned_when_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let storage_root = tmp.path().join("storage");
        let db_path = tmp.path().join("aligned.db");

        write_archive_message(&storage_root, "test-project", 1);
        write_archive_message(&storage_root, "test-project", 2);
        write_archive_message(&storage_root, "test-project", 3);
        // Write project.json so identity matches.
        std::fs::write(
            storage_root
                .join("projects")
                .join("test-project")
                .join("project.json"),
            r#"{"slug": "test-project", "human_key": "/test/project"}"#,
        )
        .unwrap();
        setup_db_with_messages(&db_path, &[1, 2, 3]);

        let report = compute_archive_drift_report(&storage_root, &db_path).unwrap();
        assert_eq!(report.archive_message_count, 3);
        assert_eq!(report.db_message_count, 3);
        assert_eq!(report.shared_message_count, 3);
        assert!(report.archive_only_ids.is_empty());
        assert!(report.db_only_ids.is_empty());
        assert!(!report.has_message_drift());
    }

    #[test]
    fn drift_report_archive_ahead() {
        let tmp = tempfile::tempdir().unwrap();
        let storage_root = tmp.path().join("storage");
        let db_path = tmp.path().join("archive_ahead.db");

        write_archive_message(&storage_root, "test-project", 1);
        write_archive_message(&storage_root, "test-project", 2);
        write_archive_message(&storage_root, "test-project", 3);
        std::fs::write(
            storage_root
                .join("projects")
                .join("test-project")
                .join("project.json"),
            r#"{"slug": "test-project", "human_key": "/test/project"}"#,
        )
        .unwrap();
        // DB only has message 1.
        setup_db_with_messages(&db_path, &[1]);

        let report = compute_archive_drift_report(&storage_root, &db_path).unwrap();
        assert_eq!(report.archive_message_count, 3);
        assert_eq!(report.db_message_count, 1);
        assert_eq!(report.shared_message_count, 1);
        assert_eq!(report.archive_only_ids, BTreeSet::from([2, 3]));
        assert!(report.db_only_ids.is_empty());
        assert!(report.has_message_drift());
        assert!(report.has_any_drift());
    }

    #[test]
    fn drift_report_db_ahead() {
        let tmp = tempfile::tempdir().unwrap();
        let storage_root = tmp.path().join("storage");
        let db_path = tmp.path().join("db_ahead.db");

        write_archive_message(&storage_root, "test-project", 1);
        std::fs::write(
            storage_root
                .join("projects")
                .join("test-project")
                .join("project.json"),
            r#"{"slug": "test-project", "human_key": "/test/project"}"#,
        )
        .unwrap();
        // DB has messages 1, 2, 3.
        setup_db_with_messages(&db_path, &[1, 2, 3]);

        let report = compute_archive_drift_report(&storage_root, &db_path).unwrap();
        assert_eq!(report.archive_message_count, 1);
        assert_eq!(report.db_message_count, 3);
        assert_eq!(report.shared_message_count, 1);
        assert!(report.archive_only_ids.is_empty());
        assert_eq!(report.db_only_ids, BTreeSet::from([2, 3]));
        assert!(report.has_message_drift());
    }

    #[test]
    fn drift_report_bidirectional_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let storage_root = tmp.path().join("storage");
        let db_path = tmp.path().join("bidir.db");

        // Archive has 1, 2, 5.
        write_archive_message(&storage_root, "test-project", 1);
        write_archive_message(&storage_root, "test-project", 2);
        write_archive_message(&storage_root, "test-project", 5);
        std::fs::write(
            storage_root
                .join("projects")
                .join("test-project")
                .join("project.json"),
            r#"{"slug": "test-project", "human_key": "/test/project"}"#,
        )
        .unwrap();
        // DB has 1, 3, 4.
        setup_db_with_messages(&db_path, &[1, 3, 4]);

        let report = compute_archive_drift_report(&storage_root, &db_path).unwrap();
        assert_eq!(report.shared_message_count, 1); // only id=1
        assert_eq!(report.archive_only_ids, BTreeSet::from([2, 5]));
        assert_eq!(report.db_only_ids, BTreeSet::from([3, 4]));
        assert!(report.has_message_drift());
    }

    #[test]
    fn drift_report_identity_mismatch_archive_project_missing_from_db() {
        let tmp = tempfile::tempdir().unwrap();
        let storage_root = tmp.path().join("storage");
        let db_path = tmp.path().join("identity_mismatch.db");

        // Archive has two projects.
        write_archive_message(&storage_root, "proj-a", 1);
        write_archive_message(&storage_root, "proj-b", 2);
        // DB only has proj-a.
        setup_db_with_messages(&db_path, &[1]);

        let report = compute_archive_drift_report(&storage_root, &db_path).unwrap();
        // proj-b should appear as an identity mismatch.
        assert!(report.has_identity_drift());
        assert!(
            report
                .identity_mismatches
                .iter()
                .any(|m| m.archive.is_some() && m.db.is_none()),
            "expected archive-only project identity mismatch"
        );
    }

    #[test]
    fn drift_report_serializes_to_json() {
        let tmp = tempfile::tempdir().unwrap();
        let storage_root = tmp.path().join("storage");
        let db_path = tmp.path().join("serialize.db");

        write_archive_message(&storage_root, "test-project", 1);
        write_archive_message(&storage_root, "test-project", 2);
        std::fs::write(
            storage_root
                .join("projects")
                .join("test-project")
                .join("project.json"),
            r#"{"slug": "test-project", "human_key": "/test/project"}"#,
        )
        .unwrap();
        setup_db_with_messages(&db_path, &[1]);

        let report = compute_archive_drift_report(&storage_root, &db_path).unwrap();
        let json = serde_json::to_value(&report).expect("should serialize");
        assert_eq!(
            json["schema"]["name"],
            "mcp-agent-mail-archive-drift-report"
        );
        assert_eq!(json["schema"]["major"], 1);
        assert_eq!(json["archive_only_ids"].as_array().unwrap().len(), 1);
        assert!(json["db_only_ids"].as_array().unwrap().is_empty());
    }

    #[test]
    fn drift_report_empty_archive_and_db() {
        let tmp = tempfile::tempdir().unwrap();
        let storage_root = tmp.path().join("empty_storage");
        let db_path = tmp.path().join("empty.db");
        // Create an empty DB with the messages table.
        setup_db_with_messages(&db_path, &[]);

        let report = compute_archive_drift_report(&storage_root, &db_path).unwrap();
        assert_eq!(report.archive_message_count, 0);
        assert_eq!(report.db_message_count, 0);
        assert_eq!(report.shared_message_count, 0);
        assert!(!report.has_any_drift());
    }

    #[test]
    fn drift_report_skips_in_memory_db_comparison_without_fabricating_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let storage_root = tmp.path().join("storage");

        write_archive_message(&storage_root, "test-project", 1);
        write_archive_message(&storage_root, "test-project", 2);
        std::fs::write(
            storage_root
                .join("projects")
                .join("test-project")
                .join("project.json"),
            r#"{"slug": "test-project", "human_key": "/test/project"}"#,
        )
        .unwrap();

        let report = compute_archive_drift_report(&storage_root, Path::new(":memory:")).unwrap();
        assert_eq!(report.archive_message_count, 2);
        assert_eq!(report.db_message_count, 0);
        assert!(report.archive_only_ids.is_empty());
        assert!(report.db_only_ids.is_empty());
        assert!(!report.has_any_drift());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("skipped") && warning.contains("in-memory")),
            "expected in-memory skip warning, got {:?}",
            report.warnings
        );
    }
}
