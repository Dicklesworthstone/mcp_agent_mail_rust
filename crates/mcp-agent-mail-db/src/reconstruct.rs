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

#[cfg(test)]
type SqliteDbConn = crate::CanonicalDbConn;

#[cfg(test)]
static FAIL_SALVAGE_MERGE_AFTER_PROJECTS: std::sync::atomic::AtomicBool =
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

fn apply_base_migrations_after_snapshot(conn: &DbConn) -> DbResult<()> {
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

    for migration in schema::schema_migrations_base() {
        if applied_ids.contains(&migration.id) {
            continue;
        }

        let already_satisfied =
            reconstruct_migration_preflight_already_satisfied(conn, &migration)?;
        if !already_satisfied {
            conn.execute_raw(&migration.up).map_err(|e| {
                DbError::Sqlite(format!(
                    "reconstruct: apply base migration {} ({}): {e}",
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
                "reconstruct: record base migration {}: {e}",
                migration.id
            ))
        })?;
        applied_ids.insert(migration.id);
    }

    Ok(())
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
    /// positive frontmatter `id` had already been recovered.
    pub duplicate_canonical_message_files: usize,
    /// Number of distinct logical message ids represented by the skipped
    /// duplicate canonical archive files.
    pub duplicate_canonical_message_ids: usize,
    /// Number of projects recovered only from a salvaged database.
    pub salvaged_projects: usize,
    /// Number of agents recovered only from a salvaged database.
    pub salvaged_agents: usize,
    /// Number of messages recovered only from a salvaged database.
    pub salvaged_messages: usize,
    /// Number of recipient rows inserted or state rows updated from a salvaged database.
    pub salvaged_recipients: usize,
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
        if self.salvaged_projects > 0
            || self.salvaged_agents > 0
            || self.salvaged_messages > 0
            || self.salvaged_recipients > 0
        {
            write!(
                f,
                "; salvaged {} projects, {} agents, {} messages ({} recipients/state updates)",
                self.salvaged_projects,
                self.salvaged_agents,
                self.salvaged_messages,
                self.salvaged_recipients
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
    // the filesystem for the caller.
    if !db_path.exists() {
        return Err(SqlError::Custom(format!(
            "collect_db_message_ids: database file not found at {}",
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
    if let Ok(content) = std::fs::read_to_string(&project_json)
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
    let content = std::fs::read_to_string(file_path)
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
/// Opens (or creates) a fresh `SQLite` database at `db_path`, runs schema
/// migrations, then walks the archive to recover data.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or if schema creation
/// fails. Individual archive files that fail to parse are skipped (counted
/// in `parse_errors`).
#[allow(clippy::too_many_lines)]
pub fn reconstruct_from_archive(db_path: &Path, storage_root: &Path) -> DbResult<ReconstructStats> {
    let mut stats = ReconstructStats::default();
    crate::pool::validate_sqlite_target_path(db_path, "reconstruct sqlite target")
        .map_err(|error| DbError::Sqlite(format!("reconstruct: {error}")))?;
    if !is_real_directory(storage_root) {
        stats.warnings.push(format!(
            "Storage root {} is missing or not a real directory",
            storage_root.display()
        ));
        return Ok(stats);
    }

    let projects_dir = storage_root.join("projects");
    if !is_real_directory(&projects_dir) {
        stats.warnings.push(format!(
            "No projects directory found at {}",
            projects_dir.display()
        ));
        return Ok(stats);
    }

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
        // can still lag newer archive-independent runtime tables such as the ATC
        // experience store. Replaying the base migrations here keeps rebuilt DBs
        // aligned with the live server schema while preflighting `ALTER TABLE`
        // additions so latest-schema columns are not duplicated.
        apply_base_migrations_after_snapshot(&conn)?;

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

        // Phase 1: Discover projects
        let mut project_dirs: Vec<(String, PathBuf)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&projects_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() || file_type.is_symlink() {
                    continue;
                }
                let Some(slug) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
                    continue;
                };
                project_dirs.push((slug, path));
            }
        }
        project_dirs.sort_by(|a, b| a.0.cmp(&b.0));

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
    tracing::info!(%stats, "database reconstruction from archive complete");
    Ok(stats)
}

/// Reconstruct the database from the Git archive and then best-effort merge
/// any additional durable state from a salvaged `SQLite` database.
///
/// This is intended for doctor/recovery flows where the primary database file
/// was unhealthy, but a directly readable salvage database could still provide
/// additional rows that never made it into the Git archive, including DB-only
/// contact/product-bus metadata.
pub fn reconstruct_from_archive_with_salvage(
    db_path: &Path,
    storage_root: &Path,
    salvage_db_path: Option<&Path>,
) -> DbResult<ReconstructStats> {
    let mut stats = reconstruct_from_archive(db_path, storage_root)?;
    if let Some(salvage_db_path) = salvage_db_path.filter(|path| is_real_file(path)) {
        match probe_salvage_database_for_merge(salvage_db_path) {
            Ok(()) => merge_salvaged_database(db_path, salvage_db_path, &mut stats)?,
            Err(error) => stats.warnings.push(format!(
                "Skipping best-effort salvage from {}: {error}",
                salvage_db_path.display()
            )),
        }
    }
    Ok(stats)
}

fn probe_salvage_database_for_merge(path: &Path) -> DbResult<()> {
    crate::pool::validate_sqlite_target_path(path, "reconstruct salvage source")
        .map_err(|error| DbError::Sqlite(format!("reconstruct salvage: {error}")))?;
    let conn = DbConn::open_file(path.to_string_lossy().as_ref()).map_err(|e| {
        DbError::Sqlite(format!(
            "reconstruct salvage: cannot open candidate {}: {e}",
            path.display()
        ))
    })?;
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

        let profile_data = match std::fs::read_to_string(&profile_path) {
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
    let content = std::fs::read_to_string(file_path)
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

    if let Some(cid) = canonical_id
        && message_id_exists(conn, cid)?
    {
        stats.record_duplicate_canonical_message(cid, file_path);
        return Ok(());
    }

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
            "SELECT COALESCE(NULLIF(TRIM(a.name), ''), '[unknown-agent-' || mr.agent_id || ']') AS name, \
                    mr.kind AS kind \
             FROM message_recipients mr \
             LEFT JOIN agents a ON a.id = mr.agent_id \
             WHERE mr.message_id = ? \
             ORDER BY CASE mr.kind WHEN 'to' THEN 0 WHEN 'cc' THEN 1 WHEN 'bcc' THEN 2 ELSE 3 END, \
                     COALESCE(NULLIF(TRIM(a.name), ''), '[unknown-agent-' || mr.agent_id || ']') COLLATE NOCASE",
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
    let reservation_data = match std::fs::read_to_string(file_path) {
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

fn message_id_exists(conn: &DbConn, message_id: i64) -> DbResult<bool> {
    let rows = conn
        .query_sync(
            "SELECT 1 AS exists_flag FROM messages WHERE id = ? LIMIT 1",
            &[Value::BigInt(message_id)],
        )
        .map_err(|e| DbError::Sqlite(format!("check message {message_id} existence: {e}")))?;
    Ok(!rows.is_empty())
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

fn placeholder_human_key_for_human_key(human_key: &str) -> Option<String> {
    let trimmed = human_key.trim();
    if trimmed.is_empty() {
        return None;
    }
    let basename = Path::new(trimmed).file_name()?.to_str()?.trim();
    if basename.is_empty() {
        return None;
    }
    Some(format!("/{basename}"))
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

#[derive(Debug, Clone)]
struct SalvageProjectIdentityRow {
    id: i64,
    slug: String,
    human_key: String,
    created_at: i64,
}

fn reconcile_placeholder_project_duplicates_after_salvage(
    conn: &DbConn,
    project_id_map: &mut HashMap<i64, i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    let rows = conn
        .query_sync(
            "SELECT id, slug, human_key, created_at FROM projects ORDER BY id",
            &[],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: query project rows for duplicate reconciliation: {e}"
            ))
        })?;

    let mut placeholder_by_token: HashMap<String, SalvageProjectIdentityRow> = HashMap::new();
    let mut placeholder_counts: HashMap<String, usize> = HashMap::new();
    let mut canonical_counts: HashMap<String, usize> = HashMap::new();
    let mut canonical_rows = Vec::new();

    for row in &rows {
        let Some(id) = row.get_named::<i64>("id").ok().filter(|value| *value > 0) else {
            continue;
        };
        let slug = row.get_named::<String>("slug").unwrap_or_default();
        let human_key = row.get_named::<String>("human_key").unwrap_or_default();
        let created_at = row.get_named::<i64>("created_at").unwrap_or_default();
        let identity = SalvageProjectIdentityRow {
            id,
            slug: slug.clone(),
            human_key: human_key.clone(),
            created_at,
        };
        if is_synthetic_project_placeholder(&slug, &human_key) {
            if let Some(token) = normalized_project_match_token(&slug) {
                *placeholder_counts.entry(token.clone()).or_insert(0) += 1;
                placeholder_by_token.entry(token).or_insert(identity);
            }
        } else if Path::new(human_key.trim()).is_absolute()
            && let Some(token) = project_basename_token_for_human_key(&human_key)
        {
            *canonical_counts.entry(token.clone()).or_insert(0) += 1;
            canonical_rows.push((token, identity));
        }
    }

    canonical_rows.sort_by_key(|(_, row)| row.created_at);

    for (token, canonical) in canonical_rows {
        if placeholder_counts.get(&token).copied().unwrap_or(0) != 1
            || canonical_counts.get(&token).copied().unwrap_or(0) != 1
        {
            continue;
        }
        let Some(placeholder) = placeholder_by_token.get(&token).cloned() else {
            continue;
        };
        if placeholder.id == canonical.id {
            continue;
        }

        for mapped_project_id in project_id_map.values_mut() {
            if *mapped_project_id == canonical.id {
                *mapped_project_id = placeholder.id;
            }
        }

        conn.execute_sync(
            "DELETE FROM projects WHERE id = ?",
            &[Value::BigInt(canonical.id)],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: remove duplicate canonical project {}: {e}",
                canonical.slug
            ))
        })?;

        let merged_created_at =
            merge_salvaged_created_at(placeholder.created_at, canonical.created_at);
        conn.execute_sync(
            "UPDATE projects SET slug = ?, human_key = ?, created_at = ? WHERE id = ?",
            &[
                Value::Text(canonical.slug.clone()),
                Value::Text(canonical.human_key.clone()),
                Value::BigInt(merged_created_at),
                Value::BigInt(placeholder.id),
            ],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: promote placeholder project {} to canonical {}: {e}",
                placeholder.slug, canonical.slug
            ))
        })?;
        if stats.salvaged_projects > 0 {
            stats.salvaged_projects -= 1;
        }

        placeholder_by_token.insert(
            token,
            SalvageProjectIdentityRow {
                id: placeholder.id,
                slug: canonical.slug,
                human_key: canonical.human_key,
                created_at: merged_created_at,
            },
        );
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
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    let existing_rows = conn
        .query_sync(
            "SELECT program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy \
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

    if next_program != current_program
        || next_model != current_model
        || next_task_description != current_task_description
        || next_inception_ts != current_inception_ts
        || next_last_active_ts != current_last_active_ts
        || next_attachments_policy != current_attachments_policy
        || next_contact_policy != current_contact_policy
    {
        conn.execute_sync(
            "UPDATE agents SET \
                 program = ?, \
                 model = ?, \
                 task_description = ?, \
                 inception_ts = ?, \
                 last_active_ts = ?, \
                 attachments_policy = ?, \
                 contact_policy = ? \
             WHERE id = ?",
            &[
                Value::Text(next_program),
                Value::Text(next_model),
                Value::Text(next_task_description),
                Value::BigInt(next_inception_ts),
                Value::BigInt(next_last_active_ts),
                Value::Text(next_attachments_policy),
                Value::Text(next_contact_policy),
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
    let salvage_conn =
        DbConn::open_file(salvage_db_path.to_string_lossy().as_ref()).map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: cannot open salvage {}: {e}",
                salvage_db_path.display()
            ))
        })?;

    let has_projects = table_exists(&salvage_conn, "projects")?;
    let has_agents = table_exists(&salvage_conn, "agents")?;
    let has_messages = table_exists(&salvage_conn, "messages")?;
    let has_recipients = table_exists(&salvage_conn, "message_recipients")?;
    let has_agent_links = table_exists(&salvage_conn, "agent_links")?;
    let has_products = table_exists(&salvage_conn, "products")?;
    let has_product_project_links = table_exists(&salvage_conn, "product_project_links")?;

    if !(has_projects
        || has_agents
        || has_messages
        || has_recipients
        || has_agent_links
        || has_products
        || has_product_project_links)
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

    let merge_result: DbResult<()> = (|| {
        let mut project_id_map: HashMap<i64, i64> = HashMap::new();
        let mut agent_id_map: HashMap<i64, i64> = HashMap::new();
        let mut product_id_map: HashMap<i64, i64> = HashMap::new();

        if has_projects {
            let project_columns = table_columns(&salvage_conn, "projects")?;
            let Some(project_select) = build_salvage_select(
                "projects",
                &project_columns,
                &["id", "slug"],
                &["human_key", "created_at"],
                stats,
                salvage_db_path,
            ) else {
                return Ok(());
            };
            let project_rows = salvage_conn
                .query_sync(
                    &format!("SELECT {project_select} FROM projects ORDER BY id"),
                    &[],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: query projects: {e}"))
                })?;

            for row in &project_rows {
                let Some(source_project_id) =
                    row.get_named::<i64>("id").ok().filter(|value| *value > 0)
                else {
                    continue;
                };
                let slug = row
                    .get_named::<String>("slug")
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if slug.is_empty() {
                    stats.warnings.push(format!(
                        "Salvage database {} had a project row with empty slug; skipping",
                        salvage_db_path.display()
                    ));
                    continue;
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
                if let Some(placeholder_human_key) = placeholder_human_key_for_human_key(&human_key)
                    && let Ok(target_project_id) = query_last_insert_or_existing_id(
                        &target_conn,
                        "projects",
                        "human_key",
                        &placeholder_human_key,
                    )
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

            reconcile_placeholder_project_duplicates_after_salvage(
                &target_conn,
                &mut project_id_map,
                stats,
            )?;

            #[cfg(test)]
            if FAIL_SALVAGE_MERGE_AFTER_PROJECTS.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(DbError::Sqlite(
                    "reconstruct salvage: forced failure after projects".to_string(),
                ));
            }
        }

        if has_agents {
            let agent_columns = table_columns(&salvage_conn, "agents")?;
            let Some(agent_select) = build_salvage_select(
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
                ],
                stats,
                salvage_db_path,
            ) else {
                return Ok(());
            };
            let agent_rows = salvage_conn
                .query_sync(
                    &format!("SELECT {agent_select} FROM agents ORDER BY id"),
                    &[],
                )
                .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: query agents: {e}")))?;

            for row in &agent_rows {
                let Some(source_agent_id) =
                    row.get_named::<i64>("id").ok().filter(|value| *value > 0)
                else {
                    continue;
                };
                let Some(source_project_id) = row
                    .get_named::<i64>("project_id")
                    .ok()
                    .filter(|value| *value > 0)
                else {
                    continue;
                };
                let Some(&target_project_id) = project_id_map.get(&source_project_id) else {
                    stats.warnings.push(format!(
                    "Salvage agent {source_agent_id} referenced missing project id {source_project_id}; skipping"
                ));
                    continue;
                };

                let name = row
                    .get_named::<String>("name")
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if name.is_empty() {
                    stats.warnings.push(format!(
                        "Salvage database {} had an agent row with empty name; skipping",
                        salvage_db_path.display()
                    ));
                    continue;
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
                     (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                        stats,
                    )?;
                }
            }
        }

        if has_agent_links {
            if project_id_map.is_empty() || agent_id_map.is_empty() {
                stats.warnings.push(format!(
                "Salvage database {} had agent_links rows but missing project/agent state prevented contact recovery",
                salvage_db_path.display()
            ));
            } else {
                let agent_link_columns = table_columns(&salvage_conn, "agent_links")?;
                if let Some(agent_link_select) =
                    build_salvage_agent_links_select(&agent_link_columns, stats, salvage_db_path)
                {
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

                    for row in &agent_link_rows {
                        let Some(source_origin_ids) = row
                            .get_named::<i64>("a_project_id")
                            .ok()
                            .filter(|value| *value > 0)
                            .zip(
                                row.get_named::<i64>("a_agent_id")
                                    .ok()
                                    .filter(|value| *value > 0),
                            )
                        else {
                            continue;
                        };
                        let Some(source_peer_ids) = row
                            .get_named::<i64>("b_project_id")
                            .ok()
                            .filter(|value| *value > 0)
                            .zip(
                                row.get_named::<i64>("b_agent_id")
                                    .ok()
                                    .filter(|value| *value > 0),
                            )
                        else {
                            continue;
                        };

                        let Some(target_origin_ids) = project_id_map
                            .get(&source_origin_ids.0)
                            .copied()
                            .zip(agent_id_map.get(&source_origin_ids.1).copied())
                        else {
                            continue;
                        };
                        let Some(target_peer_ids) = project_id_map
                            .get(&source_peer_ids.0)
                            .copied()
                            .zip(agent_id_map.get(&source_peer_ids.1).copied())
                        else {
                            continue;
                        };

                        target_conn
                        .execute_sync(
                            "INSERT OR IGNORE INTO agent_links \
                             (a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts, expires_ts) \
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                            &[
                                Value::BigInt(target_origin_ids.0),
                                Value::BigInt(target_origin_ids.1),
                                Value::BigInt(target_peer_ids.0),
                                Value::BigInt(target_peer_ids.1),
                                Value::Text(
                                    row.get_named::<String>("status")
                                        .unwrap_or_else(|_| "pending".to_string()),
                                ),
                                Value::Text(row.get_named::<String>("reason").unwrap_or_default()),
                                Value::BigInt(
                                    row.get_named::<i64>("created_ts")
                                        .unwrap_or_else(|_| crate::now_micros()),
                                ),
                                Value::BigInt(
                                    row.get_named::<i64>("updated_ts")
                                        .unwrap_or_else(|_| crate::now_micros()),
                                ),
                                row.get_named::<i64>("expires_ts")
                                    .map_or(Value::Null, Value::BigInt),
                            ],
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: insert agent_link \
                                 {}/{}->{}/{}: {e}",
                                source_origin_ids.0,
                                source_origin_ids.1,
                                source_peer_ids.0,
                                source_peer_ids.1
                            ))
                        })?;
                    }
                }
            }
        }

        if has_products {
            let product_columns = table_columns(&salvage_conn, "products")?;
            if let Some(product_select) = build_salvage_select(
                "products",
                &product_columns,
                &["id", "product_uid", "name"],
                &["created_at"],
                stats,
                salvage_db_path,
            ) {
                let product_rows = salvage_conn
                    .query_sync(
                        &format!("SELECT {product_select} FROM products ORDER BY id"),
                        &[],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!("reconstruct salvage: query products: {e}"))
                    })?;

                for row in &product_rows {
                    let Some(source_product_id) =
                        row.get_named::<i64>("id").ok().filter(|value| *value > 0)
                    else {
                        continue;
                    };
                    let product_uid = row
                        .get_named::<String>("product_uid")
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if product_uid.is_empty() {
                        stats.warnings.push(format!(
                        "Salvage database {} had a product row with empty product_uid; skipping",
                        salvage_db_path.display()
                    ));
                        continue;
                    }
                    let name = row
                        .get_named::<String>("name")
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if name.is_empty() {
                        stats.warnings.push(format!(
                            "Salvage database {} had a product row with empty name; skipping",
                            salvage_db_path.display()
                        ));
                        continue;
                    }

                    target_conn
                    .execute_sync(
                        "INSERT OR IGNORE INTO products (product_uid, name, created_at) VALUES (?, ?, ?)",
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

                    let target_product_id = query_last_insert_or_existing_id(
                        &target_conn,
                        "products",
                        "product_uid",
                        &product_uid,
                    )
                    .or_else(|_| {
                        query_last_insert_or_existing_id(&target_conn, "products", "name", &name)
                    })?;
                    product_id_map.insert(source_product_id, target_product_id);
                }
            }
        }

        if has_product_project_links {
            if product_id_map.is_empty() || project_id_map.is_empty() {
                stats.warnings.push(format!(
                "Salvage database {} had product_project_links rows but missing product/project state prevented product link recovery",
                salvage_db_path.display()
            ));
            } else {
                let product_link_columns = table_columns(&salvage_conn, "product_project_links")?;
                if let Some(product_link_select) = build_salvage_select(
                    "product_project_links",
                    &product_link_columns,
                    &["product_id", "project_id"],
                    &["created_at"],
                    stats,
                    salvage_db_path,
                ) {
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

                    for row in &product_link_rows {
                        let Some(source_product_id) = row
                            .get_named::<i64>("product_id")
                            .ok()
                            .filter(|value| *value > 0)
                        else {
                            continue;
                        };
                        let Some(source_project_id) = row
                            .get_named::<i64>("project_id")
                            .ok()
                            .filter(|value| *value > 0)
                        else {
                            continue;
                        };

                        let Some(&target_product_id) = product_id_map.get(&source_product_id)
                        else {
                            continue;
                        };
                        let Some(&target_project_id) = project_id_map.get(&source_project_id)
                        else {
                            continue;
                        };

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
            }
        }

        let mut reconstructed_recipient_agent_ids: HashMap<(i64, String), i64> = HashMap::new();
        let mut recipient_json_updates = BTreeSet::new();

        if has_messages {
            let message_columns = table_columns(&salvage_conn, "messages")?;
            if let Some(message_select) = build_salvage_select(
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
            ) {
                let message_rows = salvage_conn
                    .query_sync(
                        &format!("SELECT {message_select} FROM messages ORDER BY id"),
                        &[],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!("reconstruct salvage: query messages: {e}"))
                    })?;

                for row in &message_rows {
                    let Some(message_id) =
                        row.get_named::<i64>("id").ok().filter(|value| *value > 0)
                    else {
                        continue;
                    };
                    if message_id_exists(&target_conn, message_id)? {
                        continue;
                    }

                    let Some(source_project_id) = row
                        .get_named::<i64>("project_id")
                        .ok()
                        .filter(|value| *value > 0)
                    else {
                        continue;
                    };
                    let Some(&target_project_id) = project_id_map.get(&source_project_id) else {
                        stats.warnings.push(format!(
                        "Salvage message {message_id} referenced missing project id {source_project_id}; skipping"
                    ));
                        continue;
                    };

                    let Some(source_sender_id) = row
                        .get_named::<i64>("sender_id")
                        .ok()
                        .filter(|value| *value > 0)
                    else {
                        continue;
                    };
                    let Some(&target_sender_id) = agent_id_map.get(&source_sender_id) else {
                        stats.warnings.push(format!(
                        "Salvage message {message_id} referenced missing sender id {source_sender_id}; skipping"
                    ));
                        continue;
                    };

                    let thread_id = row
                        .get_named::<String>("thread_id")
                        .ok()
                        .and_then(|raw: String| sanitize_reconstructed_thread_id(raw.as_str()));
                    let thread_value = thread_id.map_or(Value::Null, Value::Text);
                    let (recipients_json, to_names, cc_names, bcc_names) =
                        parse_salvaged_recipients_json(
                            row.get_named::<String>("recipients_json").ok(),
                            message_id,
                            stats,
                        );
                    let attachments = parse_salvaged_attachments_json(
                        row.get_named::<String>("attachments").ok(),
                        message_id,
                        stats,
                    );

                    target_conn
                    .execute_sync(
                        "INSERT OR IGNORE INTO messages \
                         (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                        &[
                            Value::BigInt(message_id),
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
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: insert message {message_id}: {e}"
                        ))
                    })?;
                    stats.salvaged_messages += 1;

                    for name in &to_names {
                        let agent_id = ensure_agent_exists(
                            &target_conn,
                            target_project_id,
                            name,
                            &mut reconstructed_recipient_agent_ids,
                        )?;
                        insert_recipient(&target_conn, message_id, agent_id, "to")?;
                        stats.salvaged_recipients += 1;
                        recipient_json_updates.insert(message_id);
                    }
                    for name in &cc_names {
                        let agent_id = ensure_agent_exists(
                            &target_conn,
                            target_project_id,
                            name,
                            &mut reconstructed_recipient_agent_ids,
                        )?;
                        insert_recipient(&target_conn, message_id, agent_id, "cc")?;
                        stats.salvaged_recipients += 1;
                        recipient_json_updates.insert(message_id);
                    }
                    for name in &bcc_names {
                        let agent_id = ensure_agent_exists(
                            &target_conn,
                            target_project_id,
                            name,
                            &mut reconstructed_recipient_agent_ids,
                        )?;
                        insert_recipient(&target_conn, message_id, agent_id, "bcc")?;
                        stats.salvaged_recipients += 1;
                        recipient_json_updates.insert(message_id);
                    }
                }
            }
        }

        if has_recipients {
            let recipient_columns = table_columns(&salvage_conn, "message_recipients")?;
            if let Some(recipient_select) = build_salvage_select(
                "message_recipients",
                &recipient_columns,
                &["message_id", "agent_id", "kind"],
                &["read_ts", "ack_ts"],
                stats,
                salvage_db_path,
            ) {
                let recipient_rows = salvage_conn
                .query_sync(
                    &format!(
                        "SELECT {recipient_select} FROM message_recipients ORDER BY message_id, agent_id, kind"
                    ),
                    &[],
                )
                .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: query recipients: {e}")))?;

                for row in &recipient_rows {
                    let Some(message_id) = row
                        .get_named::<i64>("message_id")
                        .ok()
                        .filter(|value| *value > 0)
                    else {
                        continue;
                    };
                    if !message_id_exists(&target_conn, message_id)? {
                        continue;
                    }

                    let Some(source_agent_id) = row
                        .get_named::<i64>("agent_id")
                        .ok()
                        .filter(|value| *value > 0)
                    else {
                        continue;
                    };
                    let Some(&target_agent_id) = agent_id_map.get(&source_agent_id) else {
                        continue;
                    };
                    let kind = row
                        .get_named::<String>("kind")
                        .unwrap_or_else(|_| "to".to_string());
                    let read_ts = row.get_named::<i64>("read_ts").ok();
                    let ack_ts = row.get_named::<i64>("ack_ts").ok();
                    recipient_json_updates.insert(message_id);

                    let existing_rows = target_conn
                    .query_sync(
                        "SELECT read_ts, ack_ts FROM message_recipients \
                         WHERE message_id = ? AND agent_id = ? AND kind = ? LIMIT 1",
                        &[
                            Value::BigInt(message_id),
                            Value::BigInt(target_agent_id),
                            Value::Text(kind.clone()),
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: query recipient state for message {message_id}: {e}"
                        ))
                    })?;

                    if existing_rows.is_empty() {
                        target_conn
                        .execute_sync(
                            "INSERT OR IGNORE INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) \
                             VALUES (?, ?, ?, ?, ?)",
                            &[
                                Value::BigInt(message_id),
                                Value::BigInt(target_agent_id),
                                Value::Text(kind),
                                read_ts.map_or(Value::Null, Value::BigInt),
                                ack_ts.map_or(Value::Null, Value::BigInt),
                            ],
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: insert recipient for message {message_id}: {e}"
                            ))
                        })?;
                        stats.salvaged_recipients += 1;
                        continue;
                    }

                    let existing_row = &existing_rows[0];
                    let current_read_ts = existing_row.get_named::<i64>("read_ts").ok();
                    let current_ack_ts = existing_row.get_named::<i64>("ack_ts").ok();
                    if current_read_ts.is_none() && read_ts.is_some()
                        || current_ack_ts.is_none() && ack_ts.is_some()
                    {
                        target_conn
                        .execute_sync(
                            "UPDATE message_recipients SET \
                                 read_ts = COALESCE(read_ts, ?), \
                                 ack_ts = COALESCE(ack_ts, ?) \
                             WHERE message_id = ? AND agent_id = ? AND kind = ?",
                            &[
                                read_ts.map_or(Value::Null, Value::BigInt),
                                ack_ts.map_or(Value::Null, Value::BigInt),
                                Value::BigInt(message_id),
                                Value::BigInt(target_agent_id),
                                Value::Text(kind),
                            ],
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: update recipient state for message {message_id}: {e}"
                            ))
                        })?;
                        stats.salvaged_recipients += 1;
                    }
                }
            }
        }

        for message_id in recipient_json_updates {
            sync_reconstructed_message_recipients_json(&target_conn, message_id)?;
        }

        target_conn
            .execute_raw("REINDEX;")
            .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: REINDEX: {e}")))?;
        Ok(())
    })();

    if let Err(err) = merge_result {
        let _ = target_conn.execute_raw("ROLLBACK;");
        return Err(err);
    }
    target_conn
        .execute_raw("COMMIT;")
        .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: commit transaction: {e}")))?;
    drop(target_conn);
    crate::pool::wal_checkpoint_truncate_path(target_db_path)
        .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: checkpoint: {e}")))?;

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

    let metadata_str = match std::fs::read_to_string(&metadata_path) {
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

        let rows = conn
            .query_sync("SELECT recipients_json FROM messages WHERE id = 1", &[])
            .unwrap();
        let recipients_json: String = rows[0].get_named("recipients_json").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recipients_json).unwrap(),
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

        let rows = conn
            .query_sync("SELECT recipients_json FROM messages WHERE id = 1", &[])
            .unwrap();
        let recipients_json: String = rows[0].get_named("recipients_json").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recipients_json).unwrap(),
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
            salvaged_projects: 0,
            salvaged_agents: 0,
            salvaged_messages: 0,
            salvaged_recipients: 0,
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
        let atc_tables = conn
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND name IN ('atc_experiences', 'atc_experience_rollups') \
                 ORDER BY name",
                &[],
            )
            .expect("query ATC tables")
            .into_iter()
            .filter_map(|row| row.get_named::<String>("name").ok())
            .collect::<Vec<_>>();
        assert_eq!(
            atc_tables,
            vec![
                "atc_experience_rollups".to_string(),
                "atc_experiences".to_string()
            ],
            "reconstruct should materialize ATC base tables required by the live server"
        );
        let rollup_columns = conn
            .query_sync("PRAGMA table_info(atc_experience_rollups)", &[])
            .expect("query ATC rollup columns")
            .into_iter()
            .filter_map(|row| row.get_named::<String>("name").ok())
            .collect::<HashSet<_>>();
        for required in [
            "ewma_loss",
            "ewma_weight",
            "delay_sum_micros",
            "delay_count",
            "delay_max_micros",
        ] {
            assert!(
                rollup_columns.contains(required),
                "reconstruct should preserve ATC rollup column {required}"
            );
        }
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
    fn reconstruct_with_salvage_skips_symlinked_salvage_parent() {
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

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect(
                    "symlinked salvage parents should be skipped without failing reconstruction",
                );
        assert!(
            stats
                .warnings
                .iter()
                .any(|warning| warning.contains("symlinked path")),
            "expected symlink warning, got {:?}",
            stats.warnings
        );
        assert_eq!(stats.salvaged_projects, 0);
    }

    #[test]
    fn reconcile_placeholder_project_duplicates_promotes_archive_project_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed.db");
        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_raw(&schema::init_schema_sql_base()).unwrap();

        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::Text("flywheel_connectors".to_string()),
                Value::Text("/flywheel_connectors".to_string()),
                Value::BigInt(10),
            ],
        )
        .unwrap();
        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(2),
                Value::Text("users-jemanuel-projects-flywheel-connectors".to_string()),
                Value::Text("/Users/jemanuel/projects/flywheel_connectors".to_string()),
                Value::BigInt(1),
            ],
        )
        .unwrap();

        let mut project_id_map = HashMap::from([(100_i64, 2_i64)]);
        let mut stats = ReconstructStats {
            salvaged_projects: 1,
            ..ReconstructStats::default()
        };

        reconcile_placeholder_project_duplicates_after_salvage(
            &conn,
            &mut project_id_map,
            &mut stats,
        )
        .expect("duplicate reconciliation should succeed");

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
            "salvage database should promote project id"
        );
        assert_eq!(
            rows[0].get_named::<String>("slug").unwrap(),
            "users-jemanuel-projects-flywheel-connectors"
        );
        assert_eq!(
            rows[0].get_named::<String>("human_key").unwrap(),
            "/Users/jemanuel/projects/flywheel_connectors"
        );
        assert_eq!(
            rows[0].get_named::<i64>("created_at").unwrap(),
            1_i64,
            "salvage database should promote project created_at"
        );
    }

    #[test]
    fn reconcile_placeholder_project_duplicates_skips_ambiguous_basename_matches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed.db");
        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_raw(&schema::init_schema_sql_base()).unwrap();

        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::Text("shared".to_string()),
                Value::Text("/shared".to_string()),
                Value::BigInt(10),
            ],
        )
        .unwrap();
        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(2),
                Value::Text("tmp-one-shared".to_string()),
                Value::Text("/tmp/one/shared".to_string()),
                Value::BigInt(1),
            ],
        )
        .unwrap();
        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(3),
                Value::Text("var-two-shared".to_string()),
                Value::Text("/var/two/shared".to_string()),
                Value::BigInt(2),
            ],
        )
        .unwrap();

        let mut project_id_map = HashMap::from([(100_i64, 2_i64), (101_i64, 3_i64)]);
        let mut stats = ReconstructStats {
            salvaged_projects: 2,
            ..ReconstructStats::default()
        };

        reconcile_placeholder_project_duplicates_after_salvage(
            &conn,
            &mut project_id_map,
            &mut stats,
        )
        .expect("ambiguous duplicate reconciliation should leave distinct canonical rows alone");

        let rows = conn
            .query_sync(
                "SELECT id, slug, human_key, created_at FROM projects ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].get_named::<i64>("id").unwrap(), 1);
        assert_eq!(
            rows[0].get_named::<String>("slug").unwrap(),
            "shared".to_string()
        );
        assert_eq!(
            rows[0].get_named::<String>("human_key").unwrap(),
            "/shared".to_string()
        );
        assert_eq!(rows[0].get_named::<i64>("created_at").unwrap(), 10);
        assert_eq!(rows[1].get_named::<i64>("id").unwrap(), 2);
        assert_eq!(
            rows[1].get_named::<String>("slug").unwrap(),
            "tmp-one-shared".to_string()
        );
        assert_eq!(
            rows[1].get_named::<String>("human_key").unwrap(),
            "/tmp/one/shared".to_string()
        );
        assert_eq!(rows[1].get_named::<i64>("created_at").unwrap(), 1);
        assert_eq!(rows[2].get_named::<i64>("id").unwrap(), 3);
        assert_eq!(
            rows[2].get_named::<String>("slug").unwrap(),
            "var-two-shared".to_string()
        );
        assert_eq!(
            rows[2].get_named::<String>("human_key").unwrap(),
            "/var/two/shared".to_string()
        );
        assert_eq!(rows[2].get_named::<i64>("created_at").unwrap(), 2);
        assert_eq!(project_id_map.get(&100), Some(&2));
        assert_eq!(project_id_map.get(&101), Some(&3));
        assert_eq!(stats.salvaged_projects, 2);
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
                    (1, 11, 'to', 123, 456),
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
                 VALUES (100, 10, 100, 11, 'accepted', 'carry contact state', 7, 8, 9)",
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
            "accepted"
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
        let err =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect_err("forced late salvage failure should bubble up");
        assert!(
            err.to_string()
                .contains("reconstruct salvage: forced failure after projects"),
            "unexpected error: {err}"
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
        assert!(json["archive_only_ids"].as_array().unwrap().len() == 1);
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
