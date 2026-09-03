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
    let path_str = path.to_str().ok_or_else(|| {
        DbError::Sqlite(format!(
            "reconstruct salvage: source path {} is not valid UTF-8; refusing a lossy alias",
            path.display()
        ))
    })?;
    let config = sqlmodel_sqlite::SqliteConfig::file(path_str.to_string())
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

/// Keyset batch size for salvaging `messages` rows (br-5mnkl).
///
/// The salvage source on the automatic startup-recovery path is the unhealthy
/// live mailbox itself, so `messages` (bodies included) can be multi-GB. A
/// single full-table `SELECT` materializes every row into one `Vec<Row>` —
/// the unbounded allocation that ballooned startup recovery. Keyset
/// pagination bounds peak memory to one batch; the batch is kept small
/// because each row carries a message body.
const SALVAGE_MESSAGE_BATCH_ROWS: usize = 128;

/// Keyset batch size for salvaging `message_recipients` rows (br-5mnkl).
/// Rows are fixed-width (two ids, a kind, two timestamps), so a larger batch
/// stays small in bytes while cutting per-batch query overhead.
const SALVAGE_RECIPIENT_BATCH_ROWS: usize = 1024;

/// Keyset batch size for id-only `messages` scans (br-5mnkl). Rows are a
/// single `i64`, so the batch can be large.
const DB_MESSAGE_ID_BATCH_ROWS: usize = 4096;

/// Hard cap on itemized [`ReconstructStats::warnings`] entries (br-5mnkl).
///
/// Reconstruction pushes one warning per malformed/duplicate artifact, so a
/// pathological archive (millions of broken files) could balloon the warning
/// vector itself on the startup self-heal path. Beyond this cap warnings are
/// counted in [`ReconstructStats::suppressed_warnings`] instead of itemized;
/// all numeric counters remain exact.
const MAX_ITEMIZED_WARNINGS: usize = 10_000;

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

// Fault-injection hooks for the salvage-merge rollback tests. These are
// thread-local (not process-global): salvage merges run synchronously on the
// arming test's own thread, and a process-global flag can be consumed by any
// OTHER salvage-merge test running in parallel — which both disarms the
// arming test (its merge then succeeds and `expect_err` fails) and detonates
// an unrelated test with a forced error it never armed.
#[cfg(test)]
thread_local! {
    static FAIL_SALVAGE_MERGE_AFTER_PROJECTS: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static FAIL_SALVAGE_QUERY_MESSAGES: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

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
    if primary_db_path.as_os_str() == ":memory:" {
        return Ok(());
    }
    let primary = primary_db_path.to_str().ok_or_else(|| {
        DbError::Sqlite(format!(
            "reconstruct: primary database path {} is not valid UTF-8; cannot derive the ATC sidecar path",
            primary_db_path.display()
        ))
    })?;
    let sidecar_path = crate::pool::atc_sidecar_sqlite_path(primary);
    // Refuse a symlinked sidecar target, exactly like the primary reconstruct
    // target and the salvage source: recovery must never write through a
    // pre-planted link.
    crate::pool::validate_sqlite_target_path(Path::new(&sidecar_path), "reconstruct ATC sidecar")
        .map_err(|error| DbError::Sqlite(format!("reconstruct: {error}")))?;
    match apply_atc_sidecar_schema(&sidecar_path) {
        Ok(()) => Ok(()),
        Err(first_error)
            if !matches!(
                std::fs::symlink_metadata(&sidecar_path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ) =>
        {
            // A pre-existing sidecar that cannot be opened/migrated (the disk
            // incident that corrupted the primary DB usually hits its
            // same-directory sibling too) must NOT wedge recovery of the
            // PRIMARY mailbox: ATC telemetry is droppable by contract, while a
            // fatal abort here blocks every reconstruct retry until a human
            // intervenes. Quarantine the unusable sidecar by rename (never
            // delete) and rebuild a fresh one; only a failure on the fresh
            // file — a genuine environment problem — stays fatal.
            let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
            let quarantine_path = crate::pool::quarantine_reconstruction_candidate_path(
                Path::new(&sidecar_path),
                Path::new(&sidecar_path),
                "atc-quarantined",
                &timestamp,
            )
            .map_err(|quarantine_error| {
                DbError::Sqlite(format!(
                    "reconstruct: ATC sidecar {sidecar_path} is unusable ({first_error}) and its complete SQLite family could not be quarantined: {quarantine_error}"
                ))
            })?
            .ok_or_else(|| {
                DbError::Sqlite(format!(
                    "reconstruct: ATC sidecar {sidecar_path} failed schema application ({first_error}) but disappeared before its family could be quarantined"
                ))
            })?;
            tracing::warn!(
                sidecar = %sidecar_path,
                quarantine = %quarantine_path.display(),
                error = %first_error,
                "reconstruct: quarantined unusable ATC sidecar; rebuilding a fresh one"
            );
            apply_atc_sidecar_schema(&sidecar_path).map_err(|retry_error| {
                DbError::Sqlite(format!(
                    "reconstruct: rebuild ATC sidecar {sidecar_path} after quarantining the \
                     unusable one at {}: {retry_error}",
                    quarantine_path.display()
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
    let sidecar_path_ref = Path::new(sidecar_path);
    let preexisting = match std::fs::symlink_metadata(sidecar_path_ref) {
        Ok(metadata) if metadata.file_type().is_file() => {
            let file =
                mcp_agent_mail_core::disk::open_regular_file_for_permission_change_no_follow(
                    sidecar_path_ref,
                )
                .map_err(|error| {
                    DbError::Sqlite(format!(
                        "reconstruct: securely open existing ATC sidecar {sidecar_path}: {error}"
                    ))
                })?;
            let links = crate::pool::recovery_file_link_count(&file).map_err(|error| {
                DbError::Sqlite(format!(
                    "reconstruct: prove exclusive ownership of ATC sidecar {sidecar_path}: {error}"
                ))
            })?;
            if links != 1 {
                return Err(DbError::Sqlite(format!(
                    "reconstruct: ATC sidecar {sidecar_path} has {links} hard links; refusing to mutate a shared inode"
                )));
            }
            mcp_agent_mail_core::disk::set_private_writable_file_permissions(&file).map_err(
                |error| {
                    DbError::Sqlite(format!(
                        "reconstruct: secure existing ATC sidecar {sidecar_path}: {error}"
                    ))
                },
            )?;
            true
        }
        Ok(_) => {
            return Err(DbError::Sqlite(format!(
                "reconstruct: ATC sidecar {sidecar_path} is not a regular non-symlink file"
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            mcp_agent_mail_core::disk::create_new_private_file_no_follow(sidecar_path_ref)
                .map_err(|create_error| {
                    DbError::Sqlite(format!(
                        "reconstruct: privately claim new ATC sidecar {sidecar_path}: {create_error}"
                    ))
                })?;
            false
        }
        Err(error) => {
            return Err(DbError::Sqlite(format!(
                "reconstruct: inspect ATC sidecar {sidecar_path}: {error}"
            )));
        }
    };
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
    /// Number of messages re-inserted under a generated DB id because their
    /// canonical frontmatter id was already held by a *different* message
    /// (distinct created_ts/subject) in the *same* project — row-id reuse
    /// across DB generations. These are preserved (not skipped): only an
    /// identity-identical artifact is a true duplicate (br-r6awv).
    pub same_project_canonical_identity_collisions: usize,
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
    /// Number of salvaged rows skipped because they referenced project or
    /// agent identities absent from the salvage source itself (dangling
    /// cross-generation references, mcp_agent_mail_rust#219). One dangling
    /// coordination row must never block whole-mailbox recovery; each skip
    /// is itemized through the warning channel.
    pub salvaged_rows_skipped_unmapped: usize,
    /// Number of salvaged messages whose sender identity was missing from
    /// the salvage source and was replaced with a placeholder agent so the
    /// message content survives (the #113 "unknown-agent" doctrine).
    pub salvaged_placeholder_senders: usize,
    /// Number of archive files that failed to parse (skipped).
    pub parse_errors: usize,
    /// Human-readable warnings collected during reconstruction. Itemization
    /// is capped at [`MAX_ITEMIZED_WARNINGS`]; the overflow is counted in
    /// `suppressed_warnings` (br-5mnkl).
    pub warnings: Vec<String>,
    /// Number of warnings suppressed after `warnings` reached
    /// [`MAX_ITEMIZED_WARNINGS`]. Counters stay exact; only the per-occurrence
    /// itemization stops.
    pub suppressed_warnings: usize,
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
    /// Push an itemized warning, or count it as suppressed once the
    /// [`MAX_ITEMIZED_WARNINGS`] cap is reached (br-5mnkl). Every unbounded
    /// per-artifact/per-row warning site must route through this so the
    /// warning vector itself cannot balloon on the startup self-heal path.
    fn push_warning(&mut self, warning: String) {
        if self.warnings.len() < MAX_ITEMIZED_WARNINGS {
            self.warnings.push(warning);
        } else {
            self.suppressed_warnings = self.suppressed_warnings.saturating_add(1);
        }
    }

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

    fn record_same_project_canonical_identity_collision(
        &mut self,
        message_id: i64,
        project_id: i64,
        file_path: &Path,
    ) {
        self.same_project_canonical_identity_collisions += 1;
        if self.same_project_canonical_identity_collisions
            <= DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT
        {
            self.warnings.push(format!(
                "Canonical message id {message_id} in {} is already held by a different \
                 message (distinct created_ts/subject) in the same project_id {project_id} \
                 (row-id reuse across DB generations); inserting under a generated DB id \
                 to avoid data loss",
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
        if self.same_project_canonical_identity_collisions > 0 {
            write!(
                f,
                "; preserved {} same-project canonical id reuse collision(s) under generated DB ids",
                self.same_project_canonical_identity_collisions
            )?;
        }
        if self.suppressed_warnings > 0 {
            write!(
                f,
                "; suppressed {} warning(s) past the {MAX_ITEMIZED_WARNINGS}-entry itemization cap",
                self.suppressed_warnings
            )?;
        }
        if self.salvaged_projects > 0
            || self.salvaged_agents > 0
            || self.salvaged_messages > 0
            || self.salvaged_message_id_remaps > 0
            || self.salvaged_recipients > 0
            || self.salvaged_reservations > 0
            || self.salvaged_reservation_releases > 0
        {
            write!(
                f,
                "; salvaged {} projects, {} agents, {} messages ({} numeric-id remaps, {} recipients/state updates, {} reservations, {} reservation releases)",
                self.salvaged_projects,
                self.salvaged_agents,
                self.salvaged_messages,
                self.salvaged_message_id_remaps,
                self.salvaged_recipients,
                self.salvaged_reservations,
                self.salvaged_reservation_releases
            )?;
        }
        if self.salvaged_rows_skipped_unmapped > 0 || self.salvaged_placeholder_senders > 0 {
            write!(
                f,
                "; dropped {} unmappable salvaged row(s), substituted {} placeholder sender(s)",
                self.salvaged_rows_skipped_unmapped, self.salvaged_placeholder_senders
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

    // Engine-dispatching: the database being inventoried is frequently one
    // this module just reconstructed or a restored backup, neither of which
    // carries a FrankenSQLite namespace pair.
    let conn =
        crate::pool::open_guarded_read_only_sqlite_file(db_path, "database message-id inventory")
            .map_err(|error| SqlError::Custom(format!("collect_db_message_ids: {error}")))?;
    // Check if messages table exists.
    let tables = conn.query_sync(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='messages'",
        &[],
    )?;
    if tables.is_empty() {
        return Ok(BTreeSet::new());
    }
    // br-5mnkl: keyset-paginated read — this scans the (possibly huge) live
    // mailbox on the startup/self-heal drift-check path, and a full-table
    // SELECT materializes every row at once. The first batch has no id floor
    // so non-positive ids are still collected exactly as before; a batch that
    // fails to advance the floor (undecodable trailing ids) terminates with
    // the same silently-partial set the unchunked read produced.
    let mut ids = BTreeSet::new();
    let mut floor: Option<i64> = None;
    loop {
        let (where_clause, params): (&str, Vec<Value>) = match floor {
            Some(f) => (" WHERE id > ?", vec![Value::BigInt(f)]),
            None => ("", Vec::new()),
        };
        let rows = conn.query_sync(
            &format!(
                "SELECT id FROM messages{where_clause} ORDER BY id LIMIT {DB_MESSAGE_ID_BATCH_ROWS}"
            ),
            &params,
        )?;
        if rows.is_empty() {
            break;
        }
        let batch_len = rows.len();
        let mut advanced = false;
        for row in rows {
            if let Ok(id) = row.get_named::<i64>("id") {
                ids.insert(id);
                floor = Some(id);
                advanced = true;
            }
        }
        if batch_len < DB_MESSAGE_ID_BATCH_ROWS || !advanced {
            break;
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
    conn: &impl crate::pool::SyncQuery,
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

    match std::fs::symlink_metadata(db_path) {
        Ok(_) => {
            return Err(DbError::Sqlite(format!(
                "reconstruct: target family is already occupied at {}; reconstruction requires a fresh candidate path and never mutates an existing database generation",
                db_path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(DbError::Sqlite(format!(
                "reconstruct: cannot prove target {} is unoccupied: {error}",
                db_path.display()
            )));
        }
    }
    if !crate::pool::sqlite_recovery_candidate_is_standalone(db_path) {
        return Err(DbError::Sqlite(format!(
            "reconstruct: target {} has occupied SQLite or FrankenSQLite companion state; reconstruction requires a wholly fresh generation namespace",
            db_path.display()
        )));
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
    mcp_agent_mail_core::disk::create_new_private_file_no_follow(db_path).map_err(|error| {
        DbError::Sqlite(format!(
            "reconstruct: failed to claim fresh candidate {}: {error}",
            db_path.display()
        ))
    })?;

    // Refuse sidecars that raced with candidate admission. The newly claimed
    // empty main file is intentionally retained as evidence; callers allocate
    // unique staging names and may quarantine failed candidates.
    if !crate::pool::sqlite_recovery_candidate_is_standalone(db_path) {
        return Err(DbError::Sqlite(format!(
            "reconstruct: target companion state appeared during fresh-candidate admission beside {}; refusing to share a SQLite generation",
            db_path.display()
        )));
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
    let db_str = db_path.to_str().ok_or_else(|| {
        DbError::Sqlite(format!(
            "reconstruct: target path {} is not valid UTF-8",
            db_path.display()
        ))
    })?;
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
            stats.push_warning(format!(
                "No projects directory found at {}",
                projects_dir.display()
            ));
            if !create_empty_target {
                return Ok(stats);
            }
        }
    } else {
        stats.push_warning(format!(
            "Storage root {} is missing or not a real directory",
            storage_root.display()
        ));
        if !create_empty_target {
            return Ok(stats);
        }
    }
    project_dirs.sort_by(|a, b| a.0.cmp(&b.0));
    if project_dirs.is_empty() {
        stats.push_warning(format!(
            "No project archives found under {}",
            projects_dir.display()
        ));
        if !create_empty_target {
            return Ok(stats);
        }
    }

    claim_fresh_reconstruction_target(db_path)?;

    let conn = DbConn::open_file(db_str).map_err(|e| {
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
    // One-shot recovery path with no dispatch deadline: the generous 60s lock
    // wait is intentional (runtime connections are bounded at 20s, br-ovy6e).
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
            // NOTE: this split survives comment-only fragments only because
            // `DbConn` here is `CanonicalDbConn` (see the alias at the top of
            // this module), whose prepare tolerates comment-only SQL. Do not
            // copy this loop to a FrankenConnection context — it fails there
            // with "no SQL statement provided" (see
            // `ensure_base_schema_on_sync_connection` in the server crate).
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

        // Canonical-id collision losers, GLOBAL across all projects: inserted
        // only after every project's canonical ids are settled (br-r6awv).
        let mut deferred_messages: Vec<DeferredCollisionMessage> = Vec::new();

        // Reservation dedup state, GLOBAL across all projects (br-n8qh6): reservation
        // ids are a global AUTOINCREMENT, so an id reused across DB generations (or
        // across projects in a pre-generation archive) must not silently overwrite an
        // earlier reservation. `used_reservation_ids` tracks which archived ids we
        // have already preserved verbatim; a later artifact whose id is already taken
        // is re-inserted under a fresh id instead of collapsing via INSERT OR REPLACE.
        // `seen_reservations` dedups the two on-disk artifacts of one reservation (the
        // `id-<id>[-g<gen>].json` file and its `sha1(pattern).json` mirror) plus exact
        // re-scans, keyed by (project, generation, archived id).
        let mut used_reservation_ids: HashSet<i64> = HashSet::new();
        let mut seen_reservations: HashSet<(i64, String, i64)> = HashSet::new();
        // Stable-identity dedup, GLOBAL: one reservation can surface under legacy
        // and generation-stamped names, and later generations can carry renewed or
        // released lifecycle state for that same lease. Track the imported row by
        // stable identity so lifecycle variants merge in place instead of becoming
        // phantom fresh-id rows that collide after live-DB salvage.
        let mut seen_reservation_identities: HashMap<ReservationIdentity, i64> = HashMap::new();

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
                    &mut used_reservation_ids,
                    &mut seen_reservations,
                    &mut seen_reservation_identities,
                    &mut stats,
                )?;
            }

            // Phase 3: Discover messages for this project
            let messages_dir = project_path.join("messages");
            if is_real_directory(&messages_dir) {
                discover_messages(
                    &conn,
                    &messages_dir,
                    pid,
                    slug,
                    &mut agent_ids,
                    &mut stats,
                    &mut deferred_messages,
                )?;
            }
        }

        // Phase 3b: insert canonical-id collision losers under generated DB
        // ids. Running this only after the full walk guarantees a generated
        // id can never occupy a later file's canonical id — the failure that
        // previously dropped real messages as same-project "duplicates"
        // (br-r6awv).
        for item in &deferred_messages {
            match parse_and_insert_message(
                &conn,
                &item.file_path,
                item.project_id,
                "",
                &mut agent_ids,
                &mut stats,
                None,
            ) {
                Ok(()) => {}
                Err(e) => {
                    if conn.execute_raw("SELECT 1").is_err() {
                        return Err(e);
                    }
                    stats.parse_errors += 1;
                    stats.push_warning(format!(
                        "Failed to reconstruct deferred collision message from {}: {e}",
                        item.file_path.display()
                    ));
                }
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

struct MaterializedLiveSalvage {
    _directory: crate::pool::CanonicalSnapshotTempDir,
    path: PathBuf,
}

fn materialize_live_salvage(path: &Path) -> DbResult<MaterializedLiveSalvage> {
    let directory = crate::pool::CanonicalSnapshotTempDir::new("reconstruct-live-salvage-")
        .map_err(|error| {
            DbError::Sqlite(format!(
                "reconstruct live salvage: cannot allocate private snapshot directory: {error}"
            ))
        })?;
    let snapshot_path = directory.path().join("salvage.sqlite3");
    let snapshot_text = snapshot_path.to_str().ok_or_else(|| {
        DbError::Sqlite(format!(
            "reconstruct live salvage: private snapshot path {} is not valid UTF-8",
            snapshot_path.display()
        ))
    })?;
    // Engine-dispatching: a Franken-admitted source goes through the bound
    // same-engine opener; a source without a namespace pair (a reconstructed
    // or restored primary that the archive has since outrun) goes through
    // canonical SQLite. Both keep the source engine-enforced read-only.
    let conn = crate::pool::open_guarded_read_only_sqlite_file(
        path,
        "archive reconstruction live salvage materialization",
    )
    .map_err(|error| {
        DbError::Sqlite(format!(
            "reconstruct live salvage: cannot open source {} through guarded read-only access: {error}",
            path.display()
        ))
    })?;
    tracing::debug!(
        path = %path.display(),
        engine = conn.engine().as_str(),
        "reconstruct live salvage: materializing source through dispatched read-only engine"
    );
    // `query_only` is a connection-local SQL policy. The pager remains in the
    // engine's enforced read-only mode after this is disabled; `VACUUM INTO`
    // may write only the fresh private destination.
    conn.execute_raw("PRAGMA query_only = OFF;")
        .map_err(|error| {
            DbError::Sqlite(format!(
                "reconstruct live salvage: cannot enable private snapshot export from {}: {error}",
                path.display()
            ))
        })?;
    let destination_literal = format!("'{}'", snapshot_text.replace('\'', "''"));
    conn.execute_raw(&format!("VACUUM INTO {destination_literal}"))
        .map_err(|error| {
            DbError::Sqlite(format!(
                "reconstruct live salvage: cannot materialize {} into private snapshot {}: {error}",
                path.display(),
                snapshot_path.display()
            ))
        })?;
    // The read-only source connection drops without a checkpoint. Keep the
    // private directory alive across every subsequent canonical probe/merge.
    drop(conn);
    neutralize_private_salvage_artifact(&snapshot_path, snapshot_text)?;
    Ok(MaterializedLiveSalvage {
        _directory: directory,
        path: snapshot_path,
    })
}

/// FrankenSQLite namespace sidecars it leaves beside a `VACUUM INTO`
/// destination.
const PRIVATE_SALVAGE_FRANKEN_NAMESPACE_SUFFIXES: [&str; 2] =
    ["-fsqlite-ns-gate", "-fsqlite-ns-use"];

/// Engine-specific WAL witnesses FrankenSQLite may leave beside the
/// destination; meaningless to the canonical readers that consume it.
const PRIVATE_SALVAGE_FRANKEN_WAL_WITNESS_SUFFIXES: [&str; 2] = ["-wal-cert", "-wal-cert-head"];

/// Turn the freshly materialized, process-private salvage artifact into a
/// self-contained rollback-journal SQLite file that any engine can open.
///
/// When the dispatched source connection is FrankenSQLite, `VACUUM INTO`
/// admits the private destination into that engine's own protocol: it
/// leaves `-fsqlite-ns-gate` / `-fsqlite-ns-use` namespace records, may
/// finish the destination in WAL mode with an unpopulated `-shm`, and writes
/// its `-wal-cert` witnesses. (A canonical-served source leaves none of this;
/// every step below is then a no-op that still proves the artifact opens.) Every downstream validator and merge reader
/// (`pool::sqlite_file_passes_full_integrity_check`, the canonical salvage
/// readers) opens the artifact through the guarded canonical read-only path,
/// which refuses any Franken-admitted path outright and, past that, cannot
/// read a resting WAL whose `-shm` it may not recover read-only
/// (`readonly_shm=1`). The salvage therefore never validated and
/// archive-ahead recovery could not promote.
///
/// The destination is a fresh inode in a private snapshot directory with no
/// other referent, so it is safe to (1) retire the namespace records, (2)
/// open the copy read-write with canonical SQLite and fold any WAL frames
/// into the main file (`journal_mode = DELETE` checkpoints and removes the
/// WAL and its index), and (3) retire the engine-specific WAL witnesses.
/// Nothing is deleted: residue is renamed inside the private directory, which
/// is discarded with [`MaterializedLiveSalvage`].
///
/// Public so the CLI's doctor snapshot path can neutralize its own private
/// `VACUUM INTO` output before handing it to the guarded canonical validators
/// and the private-salvage merge, which refuse any Franken-admitted path.
///
/// # Errors
///
/// Returns an error when the artifact cannot be opened by canonical SQLite or
/// its WAL cannot be folded into the main file.
pub fn neutralize_private_salvage_artifact(
    snapshot_path: &Path,
    snapshot_text: &str,
) -> DbResult<()> {
    retire_private_salvage_residue(snapshot_path, &PRIVATE_SALVAGE_FRANKEN_NAMESPACE_SUFFIXES);
    let conn = crate::CanonicalDbConn::open_file(snapshot_text).map_err(|error| {
        DbError::Sqlite(format!(
            "reconstruct live salvage: cannot open private snapshot {} for engine-neutral checkpoint: {error}",
            snapshot_path.display()
        ))
    })?;
    conn.execute_raw("PRAGMA journal_mode = DELETE;")
        .map_err(|error| {
            DbError::Sqlite(format!(
                "reconstruct live salvage: cannot fold private snapshot {} into a self-contained rollback-journal file: {error}",
                snapshot_path.display()
            ))
        })?;
    drop(conn);
    retire_private_salvage_residue(snapshot_path, &PRIVATE_SALVAGE_FRANKEN_WAL_WITNESS_SUFFIXES);
    Ok(())
}

/// Rename FrankenSQLite residue away from the private salvage artifact so
/// no reader can mistake the artifact for a Franken-admitted family.
fn retire_private_salvage_residue(snapshot_path: &Path, suffixes: &[&str]) {
    for suffix in suffixes {
        let sidecar = mcp_agent_mail_core::disk::sqlite_sidecar_path(snapshot_path, suffix);
        if std::fs::symlink_metadata(&sidecar).is_err() {
            continue;
        }
        let retired = mcp_agent_mail_core::disk::sqlite_sidecar_path(
            snapshot_path,
            &format!("{suffix}.retired-private-salvage"),
        );
        if let Err(error) = std::fs::rename(&sidecar, &retired) {
            tracing::warn!(
                sidecar = %sidecar.display(),
                error = %error,
                "reconstruct live salvage: could not retire FrankenSQLite residue beside the private artifact"
            );
        }
    }
}

/// Reconstruct from the Git archive and merge an engine-exclusive private or
/// offline canonical `SQLite` salvage artifact.
///
/// This entry point must never receive a live primary. Live callers use
/// [`reconstruct_from_archive_with_live_salvage`] so the source inode is
/// materialized first through the guarded opener that matches its engine
/// (FrankenSQLite when the namespace sidecar pair exists, canonical
/// `SQLite` otherwise).
///
/// # Errors
///
/// Returns an error when the private salvage source cannot be validated or
/// merged, or when archive reconstruction fails.
pub fn reconstruct_from_archive_with_private_salvage(
    db_path: &Path,
    storage_root: &Path,
    private_salvage_db_path: &Path,
) -> DbResult<ReconstructStats> {
    let metadata = std::fs::symlink_metadata(private_salvage_db_path).map_err(|error| {
        DbError::Sqlite(format!(
            "reconstruct private salvage: cannot inspect source {}: {error}",
            private_salvage_db_path.display()
        ))
    })?;
    if !metadata.file_type().is_file() {
        return Err(DbError::Sqlite(format!(
            "reconstruct private salvage: source {} is not a regular file",
            private_salvage_db_path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return Err(DbError::Sqlite(format!(
                "reconstruct private salvage: source {} has {} hard links and cannot be proven process-exclusive",
                private_salvage_db_path.display(),
                metadata.nlink()
            )));
        }
    }
    reconstruct_from_archive_with_salvage(db_path, storage_root, Some(private_salvage_db_path))
}

/// Reconstruct from the Git archive while salvaging the live primary without
/// ever opening a Franken-admitted inode through canonical SQLite.
///
/// The live source is copied logically with guarded engine-enforced
/// read-only access into a private temporary database; only that private
/// inode reaches the canonical integrity and merge probes. The engine is
/// chosen by the source's namespace authority (`pool::open_guarded_read_only_sqlite_file`):
/// a Franken-admitted family is exported by FrankenSQLite, a family without a
/// namespace pair (a reconstructed or restored primary the archive has since
/// outrun) by canonical SQLite. Namespace-admission, lock, permission, and
/// other non-corruption failures refuse rather than falling back to a raw
/// open of the live source.
///
/// # Errors
///
/// Returns an error when guarded live-source admission or materialization
/// fails for a non-corruption reason, or when archive reconstruction fails.
pub fn reconstruct_from_archive_with_live_salvage(
    db_path: &Path,
    storage_root: &Path,
    live_salvage_db_path: &Path,
) -> DbResult<ReconstructStats> {
    let materialized = match materialize_live_salvage(live_salvage_db_path) {
        Ok(materialized) => materialized,
        Err(error) => {
            let message = error.to_string();
            if crate::pool::is_corruption_error_message(&message) {
                let mut stats = reconstruct_from_archive_impl(db_path, storage_root, true)?;
                stats.push_warning(format!(
                    "live salvage source {} could not be materialized because it is corrupt ({message}); rebuilt archive-only candidate",
                    live_salvage_db_path.display()
                ));
                return Ok(stats);
            }
            return Err(DbError::Sqlite(format!(
                "reconstruct live salvage source {} failed guarded materialization; refusing an archive-only candidate because DB-only coordination state could be lost: {error}",
                live_salvage_db_path.display()
            )));
        }
    };
    reconstruct_from_archive_with_private_salvage(db_path, storage_root, &materialized.path)
}

/// Reconstruct the database from the Git archive and merge any additional
/// durable state from a private salvaged `SQLite` database.
///
/// This is intended for doctor/recovery flows where the primary database file
/// was unhealthy, but a directly readable salvage database could still provide
/// additional rows that never made it into the Git archive, including DB-only
/// contact/product-bus metadata.
///
/// When a salvage path is supplied:
/// - missing, symlink, lock, and other non-corruption probe failures still
///   refuse so we cannot silently drop DB-only coordination state
/// - a present file that is not readable SQLite, or that fails full
///   `integrity_check`, degrades to archive-only (warned, still promotable)
/// - a merge that fails with a corruption-class error also degrades; the
///   merge transaction is rolled back first
///
/// Private implementation shared by the explicit public source-kind entry
/// points and the module's focused salvage tests. Production callers must use
/// one of the named live/private APIs above.
fn reconstruct_from_archive_with_salvage(
    db_path: &Path,
    storage_root: &Path,
    salvage_db_path: Option<&Path>,
) -> DbResult<ReconstructStats> {
    // Salvage is mandatory when the source is a *healthy* readable SQLite
    // file: missing/symlink/lock failures still refuse so we cannot silently
    // drop DB-only coordination state. A source that is present but not a
    // readable SQLite image — or that fails full integrity_check — must not
    // block the archive rebuild. Merging a btree-desynced live mailbox is
    // how reconstruct injected phantom identities and then refused
    // promotion (br-r6awv / br-eudur).
    let mut salvage_for_merge: Option<&Path> = None;
    let mut unreadable_salvage: Option<String> = None;
    if let Some(salvage_db_path) = salvage_db_path {
        match classify_salvage_source(salvage_db_path) {
            SalvageSourceVerdict::Mergeable => salvage_for_merge = Some(salvage_db_path),
            SalvageSourceVerdict::DegradesToArchiveOnly(reason) => {
                unreadable_salvage = Some(reason);
            }
            SalvageSourceVerdict::Refuses(reason) => return Err(DbError::Sqlite(reason)),
        }
    }

    let mut stats =
        reconstruct_from_archive_impl(db_path, storage_root, salvage_db_path.is_some())?;
    if let Some(message) = unreadable_salvage {
        stats.push_warning(format!(
            "salvage source skipped ({message}); \
             rebuilt archive-only candidate so doctor can promote a heal"
        ));
    }
    if let Some(salvage_db_path) = salvage_for_merge
        && let Err(error) = merge_salvaged_database(db_path, salvage_db_path, &mut stats)
    {
        let message = error.to_string();
        if crate::pool::is_corruption_error_message(&message) {
            // Merge rolled back. Keep the archive-only candidate so
            // `am doctor reconstruct` can promote a heal instead of
            // leaving the mailbox wedged on a corrupt-but-openable source.
            stats.push_warning(format!(
                "salvage merge from {} failed with corruption ({message}); \
                 keeping archive-only candidate so doctor can promote a heal",
                salvage_db_path.display()
            ));
        } else {
            return Err(DbError::Sqlite(format!(
                "reconstruct salvage merge from {} failed; refusing to promote the archive-only candidate because DB-only coordination state could be lost: {error}",
                salvage_db_path.display()
            )));
        }
    }
    Ok(stats)
}

/// How a reconstruct will treat a salvage source (GH#302).
///
/// Exposed so `am doctor reconstruct --dry-run` can run the *same* validation
/// the real command runs instead of promising a recovery the real command then
/// refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SalvageSourceVerdict {
    /// The source validates; its DB-only rows will be merged into the rebuild.
    Mergeable,
    /// The source is unreadable or corrupt. The rebuild proceeds archive-only
    /// and records this reason as a warning on the promotable candidate.
    DegradesToArchiveOnly(String),
    /// The source could not be validated for a non-corruption reason, so the
    /// rebuild refuses rather than silently dropping DB-only coordination
    /// state. The string is the exact error the real run would return.
    Refuses(String),
}

/// Classify a salvage source exactly as [`reconstruct_from_archive_with_private_salvage`] will.
///
/// Read-only: it probes the candidate and runs a full integrity check on it.
#[must_use]
pub fn classify_salvage_source(salvage_db_path: &Path) -> SalvageSourceVerdict {
    let refusal = |detail: &str| {
        SalvageSourceVerdict::Refuses(format!(
            "reconstruct salvage source {} failed validation; refusing an archive-only candidate because DB-only coordination state could be lost: {detail}",
            salvage_db_path.display()
        ))
    };
    match probe_salvage_database_for_merge(salvage_db_path) {
        Ok(()) => match crate::pool::sqlite_file_passes_full_integrity_check(salvage_db_path) {
            Ok(true) => SalvageSourceVerdict::Mergeable,
            Ok(false) => SalvageSourceVerdict::DegradesToArchiveOnly(format!(
                "salvage source {} failed full integrity_check",
                salvage_db_path.display()
            )),
            Err(health_error) => {
                let message = health_error.to_string();
                if crate::pool::is_corruption_error_message(&message) {
                    SalvageSourceVerdict::DegradesToArchiveOnly(message)
                } else {
                    refusal(&message)
                }
            }
        },
        Err(error) => {
            let message = error.to_string();
            if crate::pool::is_corruption_error_message(&message) {
                SalvageSourceVerdict::DegradesToArchiveOnly(message)
            } else {
                refusal(&message)
            }
        }
    }
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
            stats.push_warning(format!(
                "Archive agent directory {} has empty/invalid name; skipping profile",
                path.display()
            ));
            continue;
        };
        if agent_name != raw_agent_name {
            stats.push_warning(format!(
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
                    stats.push_warning(format!(
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
        let retired_at = parse_ts_from_json(&profile, "retired_at");
        // Older archives encoded deregistration only through the Python-style
        // tombstone plus block_all. Newer profiles carry the explicit ledger
        // timestamp, but reconstruction must preserve both representations.
        let legacy_deregistered_at = (contact_policy.eq_ignore_ascii_case("block_all")
            && crate::models::task_description_uses_reserved_deregistered_prefix(task_description))
        .then(|| {
            crate::models::deregistered_task_timestamp_micros(task_description)
                .unwrap_or(last_active_ts)
        });
        let deregistered_at =
            parse_ts_from_json(&profile, "deregistered_at").or(legacy_deregistered_at);

        conn.execute_sync(
            "INSERT OR IGNORE INTO agents \
             (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy, retired_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                retired_at.map_or(Value::Null, Value::BigInt),
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
        if let Some(deregistered_at) = deregistered_at {
            conn.execute_sync(
                "INSERT OR IGNORE INTO agent_deregistrations (agent_id, deregistered_at) \
                 VALUES (?, ?)",
                &[Value::BigInt(aid), Value::BigInt(deregistered_at)],
            )
            .map_err(|error| {
                DbError::Sqlite(format!(
                    "reconstruct: insert agent deregistration {agent_name}: {error}"
                ))
            })?;
        }
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
    deferred: &mut Vec<DeferredCollisionMessage>,
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
        match parse_and_insert_message(
            conn,
            file_path,
            project_id,
            project_slug,
            agent_ids,
            stats,
            Some(deferred),
        ) {
            Ok(()) => {}
            Err(e) => {
                // Distinguish parse errors (skip file) from DB errors (abort).
                // Probe the connection — if it's dead, propagate the error.
                if conn.execute_raw("SELECT 1").is_err() {
                    return Err(e);
                }
                stats.parse_errors += 1;
                stats.push_warning(format!(
                    "Failed to reconstruct message from {}: {e}",
                    file_path.display()
                ));
            }
        }
    }
    Ok(())
}

/// A canonical archive file whose frontmatter `id` was already occupied by a
/// *different* message when it was first visited. Insertion is deferred until
/// every canonical id across the whole archive is settled, so the generated
/// DB id it eventually receives can never occupy a later file's canonical id
/// (which would silently drop that file as a "duplicate", br-r6awv).
struct DeferredCollisionMessage {
    file_path: PathBuf,
    project_id: i64,
}

/// Parse a single archive `.md` file and insert the message into the database.
///
/// `deferred` carries the two insertion modes:
/// - `Some(queue)`: canonical pass. A frontmatter id whose DB slot is held by
///   a different message is queued instead of inserted, after recording the
///   collision in `stats`.
/// - `None`: deferred pass. The frontmatter id is ignored and the message is
///   inserted under a generated DB id.
#[allow(clippy::too_many_lines)]
fn parse_and_insert_message(
    conn: &DbConn,
    file_path: &Path,
    project_id: i64,
    _project_slug: &str,
    agent_ids: &mut HashMap<(i64, String), i64>,
    stats: &mut ReconstructStats,
    deferred: Option<&mut Vec<DeferredCollisionMessage>>,
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
    let topic = json_str(&msg, "topic")
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let importance = json_str(&msg, "importance").unwrap_or("normal");
    let ack_required = msg
        .get("ack_required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // The fallback for a file without any parseable timestamp MUST be
    // deterministic across reconstruct runs: stamping `now_micros()` gave the
    // same artifact a new identity on every rebuild, so each recovery bred a
    // fresh row (and identity-based promotion accounting could never
    // converge, br-r6awv). The canonical filename's leading UTC timestamp is
    // stable; a constant 0 is the last resort.
    let created_ts = parse_ts_from_json(&msg, "created_ts")
        .or_else(|| parse_ts_from_json(&msg, "created"))
        .or_else(|| canonical_filename_created_ts(file_path))
        .unwrap_or(0);
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
    //   True duplicate:          the id is held by a message in the same
    //                            project with the same identity (created_ts +
    //                            subject) — two archive artifacts for one
    //                            logical message. Keep the first, skip this.
    //
    //   Identity collision:      the id is held by a *different* message —
    //                            either from another project archive (#104)
    //                            or from row-id reuse across DB generations
    //                            within the same project. Both are real
    //                            messages; skipping one would drop legitimate
    //                            data. Defer this file to a second pass that
    //                            runs after every canonical id is settled, so
    //                            its generated DB id can never occupy a later
    //                            file's canonical id and cause that file to be
    //                            silently dropped as a "duplicate" (br-r6awv).
    //
    // See: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/104
    let canonical_id = match (canonical_id, deferred) {
        (Some(cid), Some(deferred)) => {
            if let Some((existing_project_id, existing_created_ts, existing_subject)) =
                message_row_identity(conn, cid)?
            {
                if existing_project_id == project_id
                    && existing_created_ts == created_ts
                    && existing_subject == subject
                {
                    stats.record_duplicate_canonical_message(cid, file_path);
                    return Ok(());
                }
                if existing_project_id == project_id {
                    stats.record_same_project_canonical_identity_collision(
                        cid, project_id, file_path,
                    );
                } else {
                    stats.record_cross_project_canonical_collision(
                        cid,
                        existing_project_id,
                        project_id,
                        file_path,
                    );
                }
                deferred.push(DeferredCollisionMessage {
                    file_path: file_path.to_path_buf(),
                    project_id,
                });
                return Ok(());
            }
            Some(cid)
        }
        // Deferred pass: the canonical id lost its slot to a different
        // message. Insert under a generated DB id, but still skip a true
        // duplicate of a message already inserted — two losers can share
        // an identity after colliding with a third occupant of the same id.
        (Some(cid), None) => {
            if message_identity_already_present(conn, project_id, created_ts, subject)? {
                stats.record_duplicate_canonical_message(cid, file_path);
                return Ok(());
            }
            None
        }
        (None, _) => None,
    };

    let thread_id = raw_thread_id.and_then(|raw| {
        let normalized = sanitize_reconstructed_thread_id(raw);
        if normalized.as_deref() != Some(raw) {
            stats.push_warning(format!(
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
    let topic_val = topic.map_or_else(|| Value::Null, |value| Value::Text(value.to_string()));

    let message_id = if let Some(cid) = canonical_id {
        // Plain INSERT: the id was verified free above. A conflict here means
        // the free-slot check was violated; failing loudly is strictly better
        // than REPLACE silently destroying an earlier message and its
        // recipient rows.
        conn.execute_sync(
            "INSERT INTO messages \
             (id, project_id, sender_id, thread_id, topic, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(cid),
                Value::BigInt(project_id),
                Value::BigInt(sender_id),
                thread_id_val,
                topic_val,
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
             (project_id, sender_id, thread_id, topic, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(project_id),
                Value::BigInt(sender_id),
                thread_id_val,
                topic_val,
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
            stats.push_warning(format!(
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
            stats.push_warning(format!(
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
        stats.push_warning(format!(
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
            stats.push_warning(format!(
                "Salvage message {message_id} has non-array attachments payload; preserving malformed attachment metadata sentinel"
            ));
            malformed_attachments_json()
        }
        Err(err) => {
            stats.push_warning(format!(
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
            stats.push_warning(format!(
                "Salvage message {message_id} has invalid recipients_json; preserving malformed recipient metadata sentinel: {err}"
            ));
            return malformed();
        }
    };
    if !reconstructed_recipients_payload_is_valid(&parsed) {
        stats.push_warning(format!(
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
    /// DB generation token this artifact was written by (br-n8qh6), from the
    /// filename (`id-<id>-g<generation>.json`) or the artifact's `db_generation`
    /// field. `None` for a legacy artifact. Used to keep cross-generation
    /// reservations that share an id from overwriting one another.
    generation: Option<String>,
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
            stats.push_warning(format!(
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
            stats.push_warning(format!(
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
        stats.push_warning(format!(
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
    // Reservation identity is (project, agent, path, exclusive, created_ts).
    // A `now_micros()` fallback minted a new identity on every recovery, so
    // promotion treated the archive replay as a different lease than the live
    // row and refused the candidate (br-r6awv). Prefer an explicit `created`
    // alias, then the deterministic last resort used by messages: 0.
    let created_ts = parse_ts_from_json(&reservation, "created_ts")
        .or_else(|| parse_ts_from_json(&reservation, "created"))
        .unwrap_or(0);
    let expires_ts = parse_ts_from_json(&reservation, "expires_ts").unwrap_or(created_ts);
    let released_ts = parse_ts_from_json(&reservation, "released_ts");
    let reservation_id = reservation
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0);

    // Generation token: prefer the filename (`id-<id>-g<generation>.json`), then
    // fall back to the artifact's own `db_generation` field so a mirror
    // (`sha1(pattern).json`) or a legacy-named-but-stamped file still attributes.
    let generation = file_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(mcp_agent_mail_core::reservation_artifact::parse_reservation_artifact_filename)
        .and_then(|parsed| parsed.generation)
        .or_else(|| json_str(&reservation, "db_generation").map(str::to_string));

    Some(ArchivedFileReservation {
        reservation_id,
        generation,
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
    used_reservation_ids: &mut HashSet<i64>,
    seen_reservations: &mut HashSet<(i64, String, i64)>,
    seen_reservation_identities: &mut HashMap<ReservationIdentity, i64>,
) -> DbResult<()> {
    let agent_id = ensure_agent_exists(conn, project_id, &reservation.agent_name, agent_ids)?;

    // Dedup the two on-disk artifacts of one reservation (id file + sha1 mirror)
    // and exact re-scans before interpreting lifecycle variants across generations.
    if let Some(id) = reservation.reservation_id {
        let generation_key = reservation.generation.clone().unwrap_or_default();
        if !seen_reservations.insert((project_id, generation_key, id)) {
            return Ok(());
        }
    }

    let stable_identity = (
        project_id,
        agent_id,
        reservation.path_pattern.clone(),
        i64::from(reservation.exclusive),
        reservation.created_ts,
    );

    // One reservation, one row: renewal and release are lifecycle payload for the
    // stable lease identity, not new leases. Merge later archive state onto the
    // already-imported row instead of minting a fresh phantom id.
    if let Some(existing_id) = seen_reservation_identities.get(&stable_identity).copied() {
        merge_archived_file_reservation_lifecycle(conn, existing_id, reservation)?;
        return Ok(());
    }

    let columns_no_id = "project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts";
    let values_no_id = [
        Value::BigInt(project_id),
        Value::BigInt(agent_id),
        Value::Text(reservation.path_pattern.clone()),
        Value::BigInt(i64::from(reservation.exclusive)),
        Value::Text(reservation.reason.clone()),
        Value::BigInt(reservation.created_ts),
        Value::BigInt(reservation.expires_ts),
        reservation.released_ts.map_or(Value::Null, Value::BigInt),
    ];

    // A fresh-id insert (no explicit id) — used for legacy no-id artifacts and for
    // any archived id already claimed by an earlier generation's reservation.
    let insert_fresh = |conn: &DbConn| -> DbResult<i64> {
        conn.execute_sync(
            &format!(
                "INSERT INTO file_reservations ({columns_no_id}) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
            ),
            &values_no_id,
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct: insert file reservation {}: {e}",
                file_path.display()
            ))
        })?;
        query_last_insert_rowid(conn)
    };

    let Some(id) = reservation.reservation_id else {
        let inserted_id = insert_fresh(conn)?;
        seen_reservation_identities.insert(stable_identity, inserted_id);
        return Ok(());
    };

    let inserted_id = if used_reservation_ids.insert(id) {
        // First artifact to claim this id — preserve it verbatim.
        conn.execute_sync(
            &format!(
                "INSERT OR REPLACE INTO file_reservations (id, {columns_no_id}) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
            ),
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
        id
    } else {
        // The archived id is already taken by an earlier (different-generation or
        // different-project) reservation — re-key under a fresh id so this row is
        // preserved instead of overwriting the earlier one (br-n8qh6).
        insert_fresh(conn)?
    };
    seen_reservation_identities.insert(stable_identity, inserted_id);
    Ok(())
}

fn merge_archived_file_reservation_lifecycle(
    conn: &DbConn,
    existing_id: i64,
    incoming: &ArchivedFileReservation,
) -> DbResult<()> {
    let rows = conn
        .query_sync(
            "SELECT reason, expires_ts, released_ts FROM file_reservations WHERE id = ? LIMIT 1",
            &[Value::BigInt(existing_id)],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct: read lifecycle for file reservation {existing_id}: {e}"
            ))
        })?;
    let row = rows.first().ok_or_else(|| {
        DbError::Sqlite(format!(
            "reconstruct: imported file reservation {existing_id} disappeared before lifecycle merge"
        ))
    })?;
    let existing_reason = row.get_named::<String>("reason").unwrap_or_default();
    let existing_expires = row.get_named::<i64>("expires_ts").map_err(|e| {
        DbError::Sqlite(format!(
            "reconstruct: decode expires_ts for file reservation {existing_id}: {e}"
        ))
    })?;
    let existing_released = row.get_named::<Option<i64>>("released_ts").map_err(|e| {
        DbError::Sqlite(format!(
            "reconstruct: decode released_ts for file reservation {existing_id}: {e}"
        ))
    })?;

    // Prefer the most advanced lifecycle artifact deterministically for free-text
    // payload, while independently preserving the greatest expiry/release clocks.
    let existing_rank = (
        existing_released.is_some(),
        existing_released.unwrap_or(existing_expires),
        existing_expires,
        existing_reason.as_str(),
    );
    let incoming_rank = (
        incoming.released_ts.is_some(),
        incoming.released_ts.unwrap_or(incoming.expires_ts),
        incoming.expires_ts,
        incoming.reason.as_str(),
    );
    let merged_reason = if incoming_rank > existing_rank {
        incoming.reason.clone()
    } else {
        existing_reason
    };
    let merged_expires = existing_expires.max(incoming.expires_ts);
    let merged_released = match (existing_released, incoming.released_ts) {
        (Some(existing), Some(incoming)) => Some(existing.max(incoming)),
        (Some(existing), None) => Some(existing),
        (None, incoming) => incoming,
    };

    conn.execute_sync(
        "UPDATE file_reservations SET reason = ?, expires_ts = ?, released_ts = ? WHERE id = ?",
        &[
            Value::Text(merged_reason),
            Value::BigInt(merged_expires),
            merged_released.map_or(Value::Null, Value::BigInt),
            Value::BigInt(existing_id),
        ],
    )
    .map_err(|e| {
        DbError::Sqlite(format!(
            "reconstruct: merge lifecycle for file reservation {existing_id}: {e}"
        ))
    })?;
    Ok(())
}

/// Stable identity of one archived file reservation. Renewal and release fields
/// are lifecycle payload and merge onto this identity rather than creating a new
/// row: (project_id, agent_id, path_pattern, exclusive, created_ts).
type ReservationIdentity = (i64, i64, String, i64, i64);

fn discover_file_reservations(
    conn: &DbConn,
    reservations_dir: &Path,
    project_id: i64,
    agent_ids: &mut HashMap<(i64, String), i64>,
    used_reservation_ids: &mut HashSet<i64>,
    seen_reservations: &mut HashSet<(i64, String, i64)>,
    seen_reservation_identities: &mut HashMap<ReservationIdentity, i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    for file_path in reservation_artifact_paths(reservations_dir) {
        let Some(reservation) = parse_archived_file_reservation(&file_path, stats) else {
            continue;
        };
        insert_archived_file_reservation(
            conn,
            project_id,
            &reservation,
            &file_path,
            agent_ids,
            used_reservation_ids,
            seen_reservations,
            seen_reservation_identities,
        )?;
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

/// Deterministic timestamp fallback for canonical archive files whose
/// frontmatter carries no parseable `created`/`created_ts`: the filename's
/// leading `%Y-%m-%dT%H-%M-%SZ` stamp (e.g.
/// `2026-04-01T13-00-00Z__subject__7.md`), in microseconds.
fn canonical_filename_created_ts(file_path: &Path) -> Option<i64> {
    let name = file_path.file_name()?.to_str()?;
    let (stamp, _) = name.split_once("__")?;
    let parsed = chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H-%M-%SZ").ok()?;
    Some(parsed.and_utc().timestamp_micros())
}

/// Owning project plus the stable identity fields (`created_ts`, `subject`)
/// of the message currently holding a DB id. Collision handling must compare
/// identity, not just id: an id can be legitimately reused by a *different*
/// message across DB generations or across separately-produced project
/// archives (br-r6awv).
fn message_row_identity(conn: &DbConn, message_id: i64) -> DbResult<Option<(i64, i64, String)>> {
    let rows = conn
        .query_sync(
            "SELECT project_id, CAST(created_ts AS INTEGER) AS created_ts, subject \
             FROM messages WHERE id = ? LIMIT 1",
            &[Value::BigInt(message_id)],
        )
        .map_err(|e| DbError::Sqlite(format!("check message {message_id} identity: {e}")))?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let pid = row
        .get_named::<i64>("project_id")
        .map_err(|e| DbError::Sqlite(format!("decode project_id for message {message_id}: {e}")))?;
    let created_ts = row
        .get_named::<i64>("created_ts")
        .map_err(|e| DbError::Sqlite(format!("decode created_ts for message {message_id}: {e}")))?;
    let subject = row.get_named::<String>("subject").unwrap_or_default();
    Ok(Some((pid, created_ts, subject)))
}

/// Find a message in `project_id` matching the stable identity fields
/// (`created_ts`, `subject`), regardless of its numeric id. Used by salvage
/// to recognize content the candidate already carries under a different id
/// (row-id reuse across DB generations, br-r6awv).
fn message_identity_existing_id(
    conn: &DbConn,
    project_id: i64,
    created_ts: i64,
    subject: &str,
) -> DbResult<Option<i64>> {
    let rows = conn
        .query_sync(
            "SELECT id FROM messages \
             WHERE project_id = ? AND CAST(created_ts AS INTEGER) = ? AND subject = ? LIMIT 1",
            &[
                Value::BigInt(project_id),
                Value::BigInt(created_ts),
                Value::Text(subject.to_string()),
            ],
        )
        .map_err(|e| DbError::Sqlite(format!("lookup message identity in project: {e}")))?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    row.get_named::<i64>("id")
        .map(Some)
        .map_err(|e| DbError::Sqlite(format!("decode message identity id: {e}")))
}

fn message_identity_already_present(
    conn: &DbConn,
    project_id: i64,
    created_ts: i64,
    subject: &str,
) -> DbResult<bool> {
    Ok(message_identity_existing_id(conn, project_id, created_ts, subject)?.is_some())
}

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
        stats.push_warning(format!(
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
    stats.push_warning(format!(
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

/// Deterministic fallback for salvage timestamps that can land in promotion
/// identity fingerprints (`products.created_at`, `product_project_links.created_at`,
/// `file_reservations.created_ts`) or that [`merge_salvaged_created_at`] already
/// treats as missing when `<= 0`. Stamping `now_micros()` here minted a new
/// identity on every recovery and refused promotion (br-r6awv).
fn salvage_ts_or_zero<E>(value: Result<i64, E>) -> i64 {
    value.unwrap_or(0)
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

/// Reconcile a slug-matched salvage row's identity against the archive-built
/// target project.
///
/// Every production salvage source is the same mailbox's previous database
/// (entry paths verify the storage root is authoritative for that sqlite
/// path), and within one archive a slug names exactly one project directory.
/// A differing non-placeholder human key therefore means the DB row drifted
/// (e.g. the repo was moved) while `project.json` holds the canonical
/// identity — the archive-side identity wins and the divergence is recorded
/// loudly. Refusing outright here (as 4b8f156c briefly did) turned the
/// reconcile self-heal into a hard failure and could never distinguish drift
/// from genuine cross-project aliasing anyway; it also matches the promotion
/// guard's GH#208 posture of refusing on identity loss only (br-uflow).
fn validate_salvage_project_identity_match(
    target_slug: &str,
    target_human_key: &str,
    salvaged_slug: &str,
    salvaged_human_key: &str,
) {
    let target_is_placeholder = is_synthetic_project_placeholder(target_slug, target_human_key);
    let salvage_is_placeholder =
        is_synthetic_project_placeholder(salvaged_slug, salvaged_human_key);
    if !target_is_placeholder
        && !salvage_is_placeholder
        && target_human_key.trim() != salvaged_human_key.trim()
    {
        tracing::warn!(
            slug = salvaged_slug,
            archive_human_key = target_human_key.trim(),
            db_human_key = salvaged_human_key.trim(),
            "reconstruct salvage: database project identity drifted from the archive's \
             canonical project.json for the same slug; salvaging rows under the archive \
             identity"
        );
    }
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
    );
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
    salvaged_retired_at: Option<i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    let existing_rows = conn
        .query_sync(
            "SELECT program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, registration_token, retired_at \
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
    let current_retired_at = existing_row
        .get_named::<Option<i64>>("retired_at")
        .unwrap_or_default();
    if salvage_has_registration_token
        && let (Some(current), Some(salvaged)) = (
            current_registration_token.as_deref(),
            salvaged_registration_token,
        )
        && current != salvaged
    {
        // Tokens are credential payload on a stable (project, name)
        // identity. Re-registration after a mailbox wipe is common; failing
        // the merge here refuses the archive-only candidate and wedges
        // doctor (br-r6awv). Keep the archive token and continue.
        stats.push_warning(format!(
            "salvaged agent {name} has a conflicting registration token; keeping the archive candidate credential"
        ));
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
    let next_registration_token = match current_registration_token.as_deref() {
        Some(_) => current_registration_token.clone(),
        None if salvage_has_registration_token => salvaged_registration_token.map(str::to_string),
        None => current_registration_token.clone(),
    };
    // Lifecycle restrictions are monotonic during salvage. The archive and
    // the readable database can represent different recovery generations;
    // clearing either side's retirement marker would silently reactivate an
    // identity. If both carry a marker, retain the first observed event.
    let next_retired_at = match (current_retired_at, salvaged_retired_at) {
        (Some(current), Some(salvaged)) => Some(current.min(salvaged)),
        (current, salvaged) => current.or(salvaged),
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
        || next_retired_at != current_retired_at
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
                 registration_token = ?, \
                 retired_at = ? \
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
                next_retired_at.map_or(Value::Null, Value::BigInt),
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
    let target_path = target_db_path.to_str().ok_or_else(|| {
        DbError::Sqlite(format!(
            "reconstruct salvage: target path {} is not valid UTF-8",
            target_db_path.display()
        ))
    })?;
    let target_conn = DbConn::open_file(target_path).map_err(|e| {
        DbError::Sqlite(format!(
            "reconstruct salvage: cannot open target {}: {e}",
            target_db_path.display()
        ))
    })?;
    let salvage_conn = open_read_only_salvage_db(salvage_db_path)?;

    let has_projects = table_exists(&salvage_conn, "projects")?;
    let has_agents = table_exists(&salvage_conn, "agents")?;
    let has_agent_deregistrations = table_exists(&salvage_conn, "agent_deregistrations")?;
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
        || has_agent_deregistrations
        || has_messages
        || has_recipients
        || has_agent_links
        || has_file_reservations
        || has_file_reservation_releases
        || has_products
        || has_product_project_links
        || has_proof_gate_consumed_nonces)
    {
        stats.push_warning(format!(
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
        // `None` means the source explicitly records a deregistration but its
        // timestamp is absent or unreadable. The state itself remains
        // authoritative; the agent's last-active timestamp supplies a stable
        // fallback when the mapped row is inserted below.
        let mut source_deregistered_agents: HashMap<i64, Option<i64>> = HashMap::new();
        if has_agent_deregistrations {
            let columns = table_columns(&salvage_conn, "agent_deregistrations")?;
            if columns.contains("agent_id") {
                let select = if columns.contains("deregistered_at") {
                    "agent_id, deregistered_at"
                } else {
                    "agent_id"
                };
                let rows = salvage_conn
                    .query_sync(
                        &format!("SELECT {select} FROM agent_deregistrations ORDER BY agent_id"),
                        &[],
                    )
                    .map_err(|error| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: query agent deregistrations: {error}"
                        ))
                    })?;
                for row in &rows {
                    let source_agent_id = row.get_named::<i64>("agent_id").map_err(|error| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode deregistered agent id: {error}"
                        ))
                    })?;
                    let deregistered_at = if columns.contains("deregistered_at") {
                        match row.get_named::<Option<i64>>("deregistered_at") {
                            Ok(value) => value,
                            Err(_) => row
                                .get_named::<Option<String>>("deregistered_at")
                                .ok()
                                .flatten()
                                .and_then(|value| {
                                    let value = value.trim();
                                    value.parse::<i64>().ok().or_else(|| {
                                        crate::iso_to_micros(&value.replacen(' ', "T", 1))
                                    })
                                }),
                        }
                    } else {
                        None
                    };
                    source_deregistered_agents.insert(source_agent_id, deregistered_at);
                }
            } else {
                stats.push_warning(format!(
                    "Salvage database {} table agent_deregistrations is missing required column agent_id",
                    salvage_db_path.display()
                ));
            }
        }
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
                let created_at = salvage_ts_or_zero(row.get_named::<i64>("created_at"));

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
            if FAIL_SALVAGE_MERGE_AFTER_PROJECTS.with(|hook| hook.replace(false)) {
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
                    "retired_at",
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
                // #219: an agent row whose project is absent from the salvage
                // source is a dangling cross-generation identity — it has no
                // representable home in the candidate. Skip it; anything that
                // references it downstream degrades per its own tier.
                let Some(target_project_id) = project_id_map.get(&source_project_id).copied()
                else {
                    stats.push_warning(format!(
                        "skipped salvaged agent {source_agent_id}: project id {source_project_id} is absent from the salvage source"
                    ));
                    stats.salvaged_rows_skipped_unmapped += 1;
                    continue;
                };

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
                let salvaged_inception_ts =
                    salvage_ts_or_zero(row.get_named::<i64>("inception_ts"));
                let salvaged_last_active_ts =
                    salvage_ts_or_zero(row.get_named::<i64>("last_active_ts"));
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
                let salvaged_retired_at = if agent_columns.contains("retired_at") {
                    match row.get_named::<Option<i64>>("retired_at") {
                        Ok(value) => value,
                        Err(integer_error) => match row.get_named::<Option<String>>("retired_at") {
                            Ok(None) => None,
                            Ok(Some(value)) => {
                                let value = value.trim();
                                value.parse::<i64>().ok().or_else(|| {
                                        crate::iso_to_micros(&value.replacen(' ', "T", 1))
                                    }).or_else(|| {
                                        stats.push_warning(format!(
                                            "salvaged agent {source_agent_id} has an unparseable retired_at value; preserving the retired state with last_active_ts"
                                        ));
                                        Some(salvaged_last_active_ts)
                                    })
                            }
                            Err(text_error) => {
                                stats.push_warning(format!(
                                        "salvaged agent {source_agent_id} has an unreadable retired_at value ({integer_error}; {text_error}); preserving the retired state with last_active_ts"
                                    ));
                                Some(salvaged_last_active_ts)
                            }
                        },
                    }
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
                     (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, registration_token, retired_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                        salvaged_retired_at.map_or(Value::Null, Value::BigInt),
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
                        salvaged_retired_at,
                        stats,
                    )?;
                }

                let legacy_deregistered_at = (salvaged_contact_policy
                    .eq_ignore_ascii_case("block_all")
                    && crate::models::task_description_uses_reserved_deregistered_prefix(
                        &salvaged_task_description,
                    ))
                .then(|| {
                    crate::models::deregistered_task_timestamp_micros(&salvaged_task_description)
                        .unwrap_or(salvaged_last_active_ts)
                });
                let deregistered_at = source_deregistered_agents
                    .remove(&source_agent_id)
                    .map(|timestamp| timestamp.unwrap_or(salvaged_last_active_ts))
                    .or(legacy_deregistered_at);
                if let Some(deregistered_at) = deregistered_at {
                    target_conn
                        .execute_sync(
                            "INSERT OR IGNORE INTO agent_deregistrations (agent_id, deregistered_at) VALUES (?, ?)",
                            &[
                                Value::BigInt(target_agent_id),
                                Value::BigInt(deregistered_at),
                            ],
                        )
                        .map_err(|error| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: insert deregistration for agent {name}: {error}"
                            ))
                        })?;
                }
            }
        }

        for source_agent_id in source_deregistered_agents.keys() {
            stats.push_warning(format!(
                "skipped salvaged deregistration for agent {source_agent_id}: agent is absent from the salvage identity map"
            ));
            stats.salvaged_rows_skipped_unmapped += 1;
        }

        if has_file_reservations {
            let reservation_columns = table_columns(&salvage_conn, "file_reservations")?;
            let reservation_select = build_salvage_select(
                "file_reservations",
                &reservation_columns,
                &["id", "project_id", "agent_id", "path_pattern"],
                &[
                    "exclusive",
                    "reason",
                    "created_ts",
                    "expires_ts",
                    "released_ts",
                ],
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
            // #219: without identity maps every reservation is unmappable by
            // definition; the per-row skip logic below handles each one
            // (leases expire on their own and must never refuse a recovery).
            if !reservation_rows.is_empty()
                && (project_id_map.is_empty() || agent_id_map.is_empty())
            {
                stats.push_warning(format!(
                    "salvage source {} has {} file_reservations row(s) but no stable project/agent identity maps; all will be skipped",
                    salvage_db_path.display(),
                    reservation_rows.len()
                ));
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
                // #219: a reservation whose project or owning agent cannot
                // be mapped is a dangling cross-generation lease. It cannot
                // be honored (its owner does not exist) and reservations
                // expire on their own; skip rather than wedge the recovery.
                let (Some(target_project_id), Some(target_agent_id)) = (
                    project_id_map.get(&source_project_id).copied(),
                    agent_id_map.get(&source_agent_id).copied(),
                ) else {
                    stats.push_warning(format!(
                        "skipped salvaged reservation {source_reservation_id}: project {source_project_id} or agent {source_agent_id} is absent from the salvage source"
                    ));
                    stats.salvaged_rows_skipped_unmapped += 1;
                    continue;
                };
                if agent_project_id(&target_conn, target_agent_id)? != Some(target_project_id) {
                    stats.push_warning(format!(
                        "skipped salvaged reservation {source_reservation_id}: agent {source_agent_id} maps outside project {source_project_id} (cross-generation artifact)"
                    ));
                    stats.salvaged_rows_skipped_unmapped += 1;
                    continue;
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
                    // Empty path is a junk row, not corruption. Failing the
                    // whole merge here refuses the archive-only candidate
                    // and wedges doctor (br-r6awv).
                    stats.push_warning(format!(
                        "skipped salvaged reservation {source_reservation_id}: empty path_pattern"
                    ));
                    stats.salvaged_rows_skipped_unmapped += 1;
                    continue;
                }
                // Exclusive is identity but historically optional on some
                // salvage schemas. Default 1 matches archive parse
                // (`unwrap_or(true)`); a hard error here is not
                // corruption-class and would refuse reconstruct.
                let exclusive = i64::from(row.get_named::<i64>("exclusive").unwrap_or(1) != 0);
                let reason = row.get_named::<String>("reason").unwrap_or_default();
                // Reservation identity includes created_ts. A missing/NULL
                // column must stay deterministic (0), not fail the whole
                // salvage merge — that error is not corruption-class, so
                // reconstruct would refuse the archive-only candidate and
                // wedge doctor (br-r6awv).
                let created_ts = salvage_ts_or_zero(row.get_named::<i64>("created_ts"));
                let expires_ts = row.get_named::<i64>("expires_ts").unwrap_or(created_ts);
                let released_ts = row.get_named::<i64>("released_ts").ok();

                // Numeric ids are local to the source database. Resolve the
                // logical reservation through remapped stable project/agent
                // identities plus the promotion identity key (path, exclusive,
                // created_ts). Exclusive belongs in the lookup: it is part of
                // reservation identity, and a mismatch must insert a second
                // lease rather than fail the whole salvage merge (that error
                // is not corruption-class and would refuse the archive-only
                // candidate, br-r6awv).
                let existing_rows = target_conn
                    .query_sync(
                        "SELECT id, exclusive, reason, expires_ts, released_ts \
                         FROM file_reservations \
                         WHERE project_id = ? AND agent_id = ? AND path_pattern = ? \
                           AND exclusive = ? AND created_ts = ? \
                         ORDER BY id",
                        &[
                            Value::BigInt(target_project_id),
                            Value::BigInt(target_agent_id),
                            Value::Text(path_pattern.clone()),
                            Value::BigInt(exclusive),
                            Value::BigInt(created_ts),
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: resolve reservation {source_reservation_id} by stable identity: {e}"
                        ))
                    })?;
                if existing_rows.len() > 1 {
                    stats.push_warning(format!(
                        "salvaged reservation {source_reservation_id} matched {} target rows for the same identity; using the earliest id and keeping the archive candidate",
                        existing_rows.len()
                    ));
                }

                let target_reservation_id = if let Some(existing) = existing_rows.first() {
                    let target_reservation_id = existing.get_named::<i64>("id").map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode target reservation id: {e}"
                        ))
                    })?;
                    let current_reason = existing.get_named::<String>("reason").unwrap_or_default();
                    let current_expires_ts = existing
                        .get_named::<i64>("expires_ts")
                        .unwrap_or(expires_ts);
                    let current_released_ts = existing.get_named::<i64>("released_ts").ok();
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

        if has_file_reservation_releases && !has_file_reservations {
            // A leftover ledger table is not corruption-class. Failing here
            // refuses the archive-only candidate and wedges doctor (br-r6awv).
            stats.push_warning(format!(
                "skipped salvaged file_reservation_releases in {}: file_reservations table is absent",
                salvage_db_path.display()
            ));
        } else if has_file_reservation_releases {
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
                // #219: a release ledger row for a reservation that was not
                // carried into the candidate (e.g. its owner was a dangling
                // cross-generation identity) has nothing to release — skip.
                let Some(target_reservation_id) =
                    reservation_id_map.get(&source_reservation_id).copied()
                else {
                    stats.push_warning(format!(
                        "skipped salvaged reservation release for reservation {source_reservation_id}: reservation was not carried into the candidate"
                    ));
                    stats.salvaged_rows_skipped_unmapped += 1;
                    continue;
                };
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
                        // released_ts is volatile lifecycle state (GH#208).
                        // A ledger mismatch is not corruption-class: keep
                        // the archive candidate's timestamp and continue so
                        // reconstruct cannot refuse the whole mailbox.
                        stats.push_warning(format!(
                            "salvaged reservation {source_reservation_id} has a conflicting terminal release ledger timestamp ({released_ts} versus candidate {current_released_ts}); keeping the archive candidate"
                        ));
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
                    stats.push_warning(format!(
                        "salvaged reservation {source_reservation_id} has a conflicting row/ledger release timestamp ({legacy_release} versus {released_ts}); keeping the archive candidate"
                    ));
                    continue;
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
            // #219: without identity maps every link is unmappable by
            // definition. The per-row skip logic below handles each one
            // (regenerable handshake state must never refuse a recovery);
            // this blanket note just makes the cause obvious in the log.
            if !agent_link_rows.is_empty() && (project_id_map.is_empty() || agent_id_map.is_empty())
            {
                stats.push_warning(format!(
                    "salvage source {} has {} agent_link row(s) but no stable project/agent identity maps; all will be skipped",
                    salvage_db_path.display(),
                    agent_link_rows.len()
                ));
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
                // #219: an agent_link whose endpoints cannot be mapped is a
                // dangling cross-generation artifact (a write that raced a
                // recovery promotion referenced identities the salvage
                // source never held). Auto-handshake links regenerate on the
                // next send; skipping one row is strictly better than
                // refusing the entire recovery with "unmapped origin agent"
                // forever. Skips are itemized and counted.
                let link_label = format!(
                    "{source_origin_project_id}/{source_origin_agent_id}->{source_peer_project_id}/{source_peer_agent_id}"
                );
                let mapped_endpoints = (
                    project_id_map.get(&source_origin_project_id).copied(),
                    agent_id_map.get(&source_origin_agent_id).copied(),
                    project_id_map.get(&source_peer_project_id).copied(),
                    agent_id_map.get(&source_peer_agent_id).copied(),
                );
                let (
                    Some(target_origin_project_id),
                    Some(target_origin_agent_id),
                    Some(target_peer_project_id),
                    Some(target_peer_agent_id),
                ) = mapped_endpoints
                else {
                    stats.push_warning(format!(
                        "skipped salvaged agent_link {link_label}: one or more endpoints reference identities absent from the salvage source (dangling cross-generation row)"
                    ));
                    stats.salvaged_rows_skipped_unmapped += 1;
                    continue;
                };
                if agent_project_id(&target_conn, target_origin_agent_id)?
                    != Some(target_origin_project_id)
                    || agent_project_id(&target_conn, target_peer_agent_id)?
                        != Some(target_peer_project_id)
                {
                    stats.push_warning(format!(
                        "skipped salvaged agent_link {link_label}: ownership crosses a stable project boundary (cross-generation artifact)"
                    ));
                    stats.salvaged_rows_skipped_unmapped += 1;
                    continue;
                }

                let link_status = row
                    .get_named::<String>("status")
                    .unwrap_or_else(|_| "pending".to_string());
                let reason = row.get_named::<String>("reason").unwrap_or_default();
                // created_ts is optional on legacy agent_links. A missing
                // column must stay deterministic (0), not `now_micros()`, but
                // must not trip the "invalid timestamp" skip that is meant
                // for present-and-broken values.
                let decoded_created_ts = row.get_named::<i64>("created_ts").ok();
                let created_ts = decoded_created_ts.unwrap_or(0);
                let updated_ts = row.get_named::<i64>("updated_ts").unwrap_or(created_ts);
                let expires_ts = row.get_named::<i64>("expires_ts").ok();
                if decoded_created_ts.is_some_and(|ts| ts <= 0) || updated_ts < created_ts {
                    stats.push_warning(format!(
                        "skipped salvaged agent_link {link_label}: invalid timestamp ordering ({created_ts}, {updated_ts})"
                    ));
                    stats.salvaged_rows_skipped_unmapped += 1;
                    continue;
                }

                let existing_links = target_conn
                    .query_sync(
                        "SELECT id FROM agent_links \
                         WHERE a_project_id = ? AND a_agent_id = ? \
                           AND b_project_id = ? AND b_agent_id = ? \
                         ORDER BY id LIMIT 2",
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
                    // Duplicate endpoint quartets are data dups, not
                    // corruption. Failing the merge here refuses the
                    // archive-only candidate and wedges doctor (br-r6awv).
                    stats.push_warning(
                        "salvaged agent_link matched multiple target rows for the same endpoint quartet; using the earliest id and keeping the archive candidate"
                            .to_string(),
                    );
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
                                Value::BigInt(salvage_ts_or_zero(
                                    row.get_named::<i64>("created_at"),
                                )),
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
            // #219: without identity maps every product link is unmappable by
            // definition; the per-row skip logic below handles each one.
            if !product_link_rows.is_empty()
                && (product_id_map.is_empty() || project_id_map.is_empty())
            {
                stats.push_warning(format!(
                    "salvage source {} has {} product_project_links row(s) but no stable product/project identity maps; all will be skipped",
                    salvage_db_path.display(),
                    product_link_rows.len()
                ));
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
                // #219: dangling product/project references are
                // cross-generation artifacts; skip the link row.
                let (Some(target_product_id), Some(target_project_id)) = (
                    product_id_map.get(&source_product_id).copied(),
                    project_id_map.get(&source_project_id).copied(),
                ) else {
                    stats.push_warning(format!(
                        "skipped salvaged product_project_link {source_product_id}->{source_project_id}: product or project is absent from the salvage source"
                    ));
                    stats.salvaged_rows_skipped_unmapped += 1;
                    continue;
                };

                target_conn
                        .execute_sync(
                            "INSERT OR IGNORE INTO product_project_links (product_id, project_id, created_at) VALUES (?, ?, ?)",
                            &[
                                Value::BigInt(target_product_id),
                                Value::BigInt(target_project_id),
                                Value::BigInt(salvage_ts_or_zero(
                                    row.get_named::<i64>("created_at"),
                                )),
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
                    "topic",
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
            if FAIL_SALVAGE_QUERY_MESSAGES.with(|hook| hook.replace(false)) {
                return Err(DbError::Sqlite(
                    "reconstruct salvage: query messages: Query error: database disk image is malformed"
                        .to_owned(),
                ));
            }

            // br-5mnkl: keyset-paginated read — see SALVAGE_MESSAGE_BATCH_ROWS.
            // The first batch has no id floor so non-positive ids (which the
            // strict salvage contract refuses) still sort first, surface, and
            // fail closed exactly as the previous full-table read did.
            // #219: memo for placeholder senders substituted for unmapped ids.
            let mut placeholder_sender_memo: HashMap<(i64, String), i64> = HashMap::new();
            let mut message_keyset_floor: Option<i64> = None;
            loop {
                let (message_where, message_params): (&str, Vec<Value>) = match message_keyset_floor
                {
                    Some(floor) => (" WHERE id > ?", vec![Value::BigInt(floor)]),
                    None => ("", Vec::new()),
                };
                let message_rows = salvage_conn
                    .query_sync(
                        &format!(
                            "SELECT {message_select} FROM messages{message_where} \
                             ORDER BY id LIMIT {SALVAGE_MESSAGE_BATCH_ROWS}"
                        ),
                        &message_params,
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!("reconstruct salvage: query messages: {e}"))
                    })?;
                if message_rows.is_empty() {
                    break;
                }

                for row in &message_rows {
                    let source_message_id = row.get_named::<i64>("id").map_err(|e| {
                        DbError::Sqlite(format!("reconstruct salvage: decode message id: {e}"))
                    })?;
                    if source_message_id <= 0 {
                        return Err(DbError::Sqlite(format!(
                            "reconstruct salvage: message has non-positive id {source_message_id}"
                        )));
                    }
                    message_keyset_floor = Some(source_message_id);
                    let source_project_id = row.get_named::<i64>("project_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode project_id for message {source_message_id}: {e}"
                    ))
                })?;
                    // #219: a message whose project identity is absent from
                    // the salvage source has no valid destination — skip it
                    // (the canonical archive copy, when one exists, is
                    // already in the candidate; the salvage source itself is
                    // preserved via quarantine/forensics).
                    let Some(target_project_id) = project_id_map.get(&source_project_id).copied()
                    else {
                        stats.push_warning(format!(
                            "skipped salvaged message {source_message_id}: project id {source_project_id} is absent from the salvage source"
                        ));
                        stats.salvaged_rows_skipped_unmapped += 1;
                        continue;
                    };
                    // Dedup BEFORE sender resolution: when the candidate
                    // (built from the archive, the primary source) already
                    // carries this message, no placeholder agent must be
                    // minted for it — the placeholder INSERT is not rolled
                    // back by a `continue`, and an unreferenced
                    // `unknown-agent-N` row would survive into the recovered
                    // database while the substitution counter overcounted.
                    //
                    // "Already carries" must be decided on identity
                    // (project + created_ts + subject), not numeric id alone:
                    // the candidate slot can hold a *different* message in the
                    // same project (canonical-id collision handling, row-id
                    // reuse across generations). Treating that as "carried"
                    // silently discards the salvaged message (br-r6awv);
                    // identity mismatches fall through to the remap path.
                    let source_subject = row.get_named::<String>("subject").unwrap_or_default();
                    // Must be deterministic: `now_micros()` here would give the
                    // same salvage row a new identity on every recovery and
                    // prevent matching the archive copy (br-r6awv).
                    let source_created_ts = row.get_named::<i64>("created_ts").unwrap_or(0);
                    if let Some((existing_pid, existing_created_ts, existing_subject)) =
                        message_row_identity(&target_conn, source_message_id)?
                        && existing_pid == target_project_id
                        && existing_created_ts == source_created_ts
                        && existing_subject == source_subject
                    {
                        message_id_map.insert(source_message_id, source_message_id);
                        continue;
                    }
                    // The same message can already be in the candidate under a
                    // DIFFERENT id: row-id reuse across DB generations means
                    // the archive's canonical file for this content carries
                    // another numeric id. Re-inserting it here would replay a
                    // duplicate, which the promotion guard rightly refuses
                    // (br-r6awv). Map to the existing row instead.
                    if let Some(existing_id) = message_identity_existing_id(
                        &target_conn,
                        target_project_id,
                        source_created_ts,
                        &source_subject,
                    )? {
                        message_id_map.insert(source_message_id, existing_id);
                        continue;
                    }

                    let source_sender_id = row.get_named::<i64>("sender_id").map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: decode sender_id for message {source_message_id}: {e}"
                    ))
                })?;
                    // #219: a sender identity missing from the salvage source
                    // must not drop the message (that is real content) and
                    // must not wedge the recovery. Substitute a placeholder
                    // agent in the target project, mirroring the #113
                    // "unknown-agent" doctrine for orphaned recipients.
                    let target_sender_id = if let Some(mapped) =
                        agent_id_map.get(&source_sender_id).copied()
                    {
                        if agent_project_id(&target_conn, mapped)? != Some(target_project_id) {
                            stats.push_warning(format!(
                                "skipped salvaged message {source_message_id}: sender {source_sender_id} maps outside project {source_project_id} (cross-generation artifact); the canonical archive copy is authoritative"
                            ));
                            stats.salvaged_rows_skipped_unmapped += 1;
                            continue;
                        }
                        mapped
                    } else {
                        let placeholder_name = format!("unknown-agent-{source_sender_id}");
                        let placeholder_id = ensure_agent_exists(
                            &target_conn,
                            target_project_id,
                            &placeholder_name,
                            &mut placeholder_sender_memo,
                        )?;
                        stats.push_warning(format!(
                                "salvaged message {source_message_id} sender id {source_sender_id} is absent from the salvage source; substituted placeholder agent '{placeholder_name}'"
                            ));
                        stats.salvaged_placeholder_senders += 1;
                        placeholder_id
                    };

                    let thread_id = row
                        .get_named::<String>("thread_id")
                        .ok()
                        .and_then(|raw: String| sanitize_reconstructed_thread_id(raw.as_str()));
                    let thread_value = thread_id.map_or(Value::Null, Value::Text);
                    let topic_value = row
                        .get_named::<String>("topic")
                        .ok()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                        .map_or(Value::Null, Value::Text);
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
                        topic_value,
                        Value::Text(source_subject),
                        Value::Text(row.get_named::<String>("body_md").unwrap_or_default()),
                        Value::Text(
                            row.get_named::<String>("importance")
                                .unwrap_or_else(|_| "normal".to_string()),
                        ),
                        Value::BigInt(i64::from(
                            row.get_named::<i64>("ack_required").unwrap_or(0) != 0,
                        )),
                        Value::BigInt(source_created_ts),
                        Value::Text(recipients_json),
                        Value::Text(attachments),
                    ];
                    let existing_project_id = message_project_id(&target_conn, source_message_id)?;
                    let target_message_id = if let Some(existing_project_id) = existing_project_id {
                        target_conn
                        .execute_sync(
                            "INSERT INTO messages \
                             (project_id, sender_id, thread_id, topic, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                            &values,
                        )
                        .map_err(|e| {
                            DbError::Sqlite(format!(
                                "reconstruct salvage: remap cross-project message {source_message_id}: {e}"
                            ))
                        })?;
                        let remapped_id = query_last_insert_rowid(&target_conn)?;
                        stats.salvaged_message_id_remaps += 1;
                        stats.push_warning(format!(
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
                             (id, project_id, sender_id, thread_id, topic, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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

                    for (names, kind) in [(&to_names, "to"), (&cc_names, "cc"), (&bcc_names, "bcc")]
                    {
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
                if message_rows.len() < SALVAGE_MESSAGE_BATCH_ROWS {
                    break;
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
            // br-5mnkl: keyset-paginated read — one row per recipient of every
            // message in the mailbox, so a full-table SELECT is unbounded.
            // Every product schema has declared `message_recipients` as an
            // ordinary rowid table (its composite PRIMARY KEY never uses
            // WITHOUT ROWID) and SQLite rowids are >= 1, so a floor of 0 is
            // exact and no row can be skipped or double-read across batches
            // even when a legacy salvage source carries duplicate
            // (message_id, agent_id) pairs. Per-row handling is
            // order-independent, so rowid order is as deterministic as the
            // previous composite ORDER BY.
            let mut recipient_rowid_floor = 0i64;
            loop {
                let recipient_rows = salvage_conn
                    .query_sync(
                        &format!(
                            "SELECT rowid AS salvage_rowid, {recipient_select} \
                             FROM message_recipients WHERE rowid > ? \
                             ORDER BY rowid LIMIT {SALVAGE_RECIPIENT_BATCH_ROWS}"
                        ),
                        &[Value::BigInt(recipient_rowid_floor)],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!("reconstruct salvage: query recipients: {e}"))
                    })?;
                if recipient_rows.is_empty() {
                    break;
                }

                for row in &recipient_rows {
                    recipient_rowid_floor = row.get_named::<i64>("salvage_rowid").map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: decode recipient salvage rowid: {e}"
                        ))
                    })?;
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
                    // #219: recipient rows carry per-recipient read/ack
                    // state. A dangling agent or message reference is a
                    // cross-generation artifact; losing one recipient's read
                    // state is trivial next to refusing the whole recovery.
                    let Some(target_agent_id) = agent_id_map.get(&source_agent_id).copied() else {
                        stats.push_warning(format!(
                            "skipped salvaged recipient state for message {source_message_id}: agent id {source_agent_id} is absent from the salvage source"
                        ));
                        stats.salvaged_rows_skipped_unmapped += 1;
                        continue;
                    };
                    let target_agent_project_id = agent_project_id(&target_conn, target_agent_id)?
                    .ok_or_else(|| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: mapped target agent {target_agent_id} is missing"
                        ))
                    })?;
                    let Some(target_message_id) = message_id_map.get(&source_message_id).copied()
                    else {
                        stats.push_warning(format!(
                            "skipped salvaged recipient state: message id {source_message_id} was not carried into the candidate"
                        ));
                        stats.salvaged_rows_skipped_unmapped += 1;
                        continue;
                    };
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
                        stats.push_warning(format!(
                            "skipped salvaged recipient state for message {source_message_id}: agent {source_agent_id} maps outside the message project (cross-generation artifact)"
                        ));
                        stats.salvaged_rows_skipped_unmapped += 1;
                        continue;
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
                        // #219: a to/cc divergence between the archive-built
                        // candidate and the live database must never refuse
                        // an entire recovery. The live DB wins: the
                        // promotion continuity receipt keys recipients on
                        // (message, agent, kind), so preserving the salvage
                        // side's kind is what "no DB coordination-state
                        // loss" means to the system's own invariants.
                        stats.push_warning(format!(
                            "salvaged recipient ({target_message_id}, {target_agent_id}) has conflicting kinds {current_kind:?} (candidate) and {kind:?} (salvage); adopted the salvage kind to preserve DB coordination-state continuity"
                        ));
                    }
                    let current_read_ts = existing_row
                        .get_named::<Option<i64>>("read_ts")
                        .unwrap_or_default();
                    let current_ack_ts = existing_row
                        .get_named::<Option<i64>>("ack_ts")
                        .unwrap_or_default();
                    if current_read_ts != read_ts
                        || current_ack_ts != ack_ts
                        || current_kind != kind
                    {
                        target_conn
                        .execute_sync(
                            "UPDATE message_recipients SET \
                                 kind = ?, read_ts = ?, ack_ts = ? \
                             WHERE message_id = ? AND agent_id = ?",
                            &[
                                Value::Text(kind),
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
                if recipient_rows.len() < SALVAGE_RECIPIENT_BATCH_ROWS {
                    break;
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
        // telemetry is, by design, droppable/resettable — which is why
        // ReconstructStats carries no rollup-salvage counter (br-j9prb).

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
        stats.push_warning(format!(
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
        stats.push_warning(format!(
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
            stats.push_warning(format!(
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
            stats.push_warning(format!(
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
        stats.push_warning(format!(
            "Missing/empty human_key in {}; using fallback human_key '{}'",
            metadata_path.display(),
            fallback
        ));
        return fallback;
    };

    if !Path::new(human_key).is_absolute() {
        stats.parse_errors += 1;
        stats.push_warning(format!(
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
        stats.push_warning(format!(
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
        stats.push_warning(format!(
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
        stats.push_warning(format!(
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
        stats.push_warning(format!(
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

// ============================================================================
// Bounded incremental apply of a small archive-ahead delta (GH#284)
// ============================================================================
//
// When the Git archive is a few messages ahead of a canonically healthy
// database, a full archive reconstruct (rebuild every project, agent, message
// and reservation into a candidate, then promote it) is the wrong tool: it is
// slow on a large mailbox, and its promotion guard can refuse for reasons that
// have nothing to do with the delta (GH#271). This path ingests only the
// archive messages whose canonical ids the database lacks, plus any project or
// agent rows they need, through the runtime engine inside one write
// transaction on the live database. It refuses (and leaves the database
// untouched) whenever the delta is not the simple case: too many messages,
// archive files that do not parse, or a canonical id already held by a
// different message.

/// What a bounded incremental apply did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveDeltaApplyStats {
    /// Canonical archive message ids the database lacked before the apply.
    pub missing_message_ids: usize,
    /// Messages inserted (always equal to `missing_message_ids` on success).
    pub messages_applied: usize,
    /// Archive project directories visited.
    pub projects_visited: usize,
    /// Archive files skipped as exact duplicates of a message already present.
    pub duplicate_files_skipped: usize,
}

/// Outcome of [`apply_archive_ahead_delta`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveDeltaApplyOutcome {
    /// The delta was applied and committed.
    Applied(ArchiveDeltaApplyStats),
    /// The delta is not the simple case; nothing was written. The caller
    /// falls back to a full reconstruct.
    NotApplicable(String),
}

/// Default upper bound on the number of missing messages the incremental
/// path applies; larger deltas reconstruct. `AM_ARCHIVE_DELTA_APPLY_MAX_MESSAGES`
/// overrides it and `0` disables the path.
pub const DEFAULT_ARCHIVE_DELTA_APPLY_MAX_MESSAGES: usize = 64;

#[must_use]
pub fn archive_delta_apply_max_messages() -> usize {
    mcp_agent_mail_core::config::process_env_value("AM_ARCHIVE_DELTA_APPLY_MAX_MESSAGES")
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_ARCHIVE_DELTA_APPLY_MAX_MESSAGES)
}

/// Apply the archive messages the database at `db_path` lacks, if the delta
/// is small and unambiguous.
///
/// The caller must already have established that the database is healthy and
/// that it holds the promotion barrier / mutation admission for the file; this
/// function only writes through the runtime engine in one `BEGIN IMMEDIATE`
/// transaction and rolls back on any doubt.
///
/// # Errors
///
/// Returns an error only for database failures (open, transaction, or query
/// errors). A delta that must not be applied is reported as
/// [`ArchiveDeltaApplyOutcome::NotApplicable`], not as an error.
pub fn apply_archive_ahead_delta(
    db_path: &Path,
    storage_root: &Path,
    max_messages: usize,
) -> DbResult<ArchiveDeltaApplyOutcome> {
    if max_messages == 0 {
        return Ok(ArchiveDeltaApplyOutcome::NotApplicable(
            "incremental apply disabled (AM_ARCHIVE_DELTA_APPLY_MAX_MESSAGES=0)".to_string(),
        ));
    }
    let (archive_ids, archive_parse_errors) = scan_archive_message_ids(storage_root);
    if archive_parse_errors > 0 {
        return Ok(ArchiveDeltaApplyOutcome::NotApplicable(format!(
            "{archive_parse_errors} archive message file(s) failed to parse; a partial delta must not be applied"
        )));
    }
    let db_ids = collect_db_message_ids(db_path)
        .map_err(|e| DbError::Sqlite(format!("incremental apply: collect db message ids: {e}")))?;
    let missing: BTreeSet<i64> = archive_ids.difference(&db_ids).copied().collect();
    if missing.is_empty() {
        return Ok(ArchiveDeltaApplyOutcome::NotApplicable(
            "the database already holds every canonical archive message id".to_string(),
        ));
    }
    if missing.len() > max_messages {
        return Ok(ArchiveDeltaApplyOutcome::NotApplicable(format!(
            "{} archive messages are missing from the database, above the incremental bound of {max_messages}",
            missing.len()
        )));
    }

    let projects_dir = storage_root.join("projects");
    let mut project_dirs: Vec<(String, PathBuf)> = Vec::new();
    if is_real_directory(&projects_dir)
        && let Ok(entries) = std::fs::read_dir(&projects_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_real_directory(&path) {
                continue;
            }
            let Some(slug) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            project_dirs.push((slug.to_string(), path));
        }
    }
    project_dirs.sort_by(|a, b| a.0.cmp(&b.0));

    let db_str = db_path.to_str().ok_or_else(|| {
        DbError::Sqlite(format!(
            "incremental apply refuses non-UTF-8 database path {}",
            db_path.display()
        ))
    })?;
    let conn = DbConn::open_file(db_str)
        .map_err(|e| DbError::Sqlite(format!("incremental apply: open {db_str}: {e}")))?;
    conn.execute_raw("PRAGMA busy_timeout=60000;")
        .map_err(|e| DbError::Sqlite(format!("incremental apply: busy_timeout: {e}")))?;
    conn.execute_raw("BEGIN IMMEDIATE;")
        .map_err(|e| DbError::Sqlite(format!("incremental apply: begin transaction: {e}")))?;

    let outcome = apply_delta_in_transaction(&conn, &project_dirs, &missing);
    match outcome {
        Ok(ArchiveDeltaApplyOutcome::Applied(stats)) => {
            conn.execute_raw("COMMIT;")
                .map_err(|e| DbError::Sqlite(format!("incremental apply: commit: {e}")))?;
            Ok(ArchiveDeltaApplyOutcome::Applied(stats))
        }
        Ok(ArchiveDeltaApplyOutcome::NotApplicable(reason)) => {
            let _ = conn.execute_raw("ROLLBACK;");
            Ok(ArchiveDeltaApplyOutcome::NotApplicable(reason))
        }
        Err(error) => {
            let _ = conn.execute_raw("ROLLBACK;");
            Err(error)
        }
    }
}

fn apply_delta_in_transaction(
    conn: &DbConn,
    project_dirs: &[(String, PathBuf)],
    missing: &BTreeSet<i64>,
) -> DbResult<ArchiveDeltaApplyOutcome> {
    let mut stats = ReconstructStats::default();
    let mut agent_ids: HashMap<(i64, String), i64> = HashMap::new();
    let mut deferred: Vec<DeferredCollisionMessage> = Vec::new();
    let mut projects_visited = 0usize;
    let mut candidate_files: Vec<(i64, PathBuf, i64, String)> = Vec::new();

    for (slug, project_path) in project_dirs {
        let messages_dir = project_path.join("messages");
        let mut project_files = Vec::new();
        collect_message_files_with_ids(&messages_dir, missing, &mut project_files)?;
        if project_files.is_empty() {
            continue;
        }
        projects_visited += 1;
        let now = crate::now_micros();
        let human_key = read_project_human_key(project_path, slug, &mut stats);
        conn.execute_sync(
            "INSERT OR IGNORE INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
            &[
                Value::Text(slug.clone()),
                Value::Text(human_key),
                Value::BigInt(now),
            ],
        )
        .map_err(|e| DbError::Sqlite(format!("incremental apply: insert project {slug}: {e}")))?;
        let pid = query_last_insert_or_existing_id(conn, "projects", "slug", slug)?;
        let agents_dir = project_path.join("agents");
        if is_real_directory(&agents_dir) {
            discover_agents(conn, &agents_dir, pid, &mut agent_ids, &mut stats)?;
        }
        for (id, file) in project_files {
            candidate_files.push((id, file, pid, slug.clone()));
        }
    }

    let expected = missing.len();
    let found: BTreeSet<i64> = candidate_files.iter().map(|(id, ..)| *id).collect();
    if found.len() != expected {
        return Ok(ArchiveDeltaApplyOutcome::NotApplicable(format!(
            "only {} of the {expected} missing canonical ids map to a single archive file",
            found.len()
        )));
    }
    candidate_files.sort_by(|a, b| a.1.cmp(&b.1));
    for (_, file_path, pid, slug) in &candidate_files {
        parse_and_insert_message(
            conn,
            file_path,
            *pid,
            slug,
            &mut agent_ids,
            &mut stats,
            Some(&mut deferred),
        )?;
    }
    if !deferred.is_empty() {
        return Ok(ArchiveDeltaApplyOutcome::NotApplicable(format!(
            "{} archive message(s) carry a canonical id already held by a different live message; refusing an ambiguous partial apply",
            deferred.len()
        )));
    }
    if stats.parse_errors > 0 {
        return Ok(ArchiveDeltaApplyOutcome::NotApplicable(format!(
            "{} archive message file(s) failed to parse during the apply",
            stats.parse_errors
        )));
    }
    let mut applied = 0usize;
    for id in missing {
        let rows = conn
            .query_sync(
                "SELECT 1 AS present FROM messages WHERE id = ?",
                &[Value::BigInt(*id)],
            )
            .map_err(|e| DbError::Sqlite(format!("incremental apply: verify id {id}: {e}")))?;
        if !rows.is_empty() {
            applied += 1;
        }
    }
    if applied != expected {
        return Ok(ArchiveDeltaApplyOutcome::NotApplicable(format!(
            "only {applied} of {expected} missing messages were applied; refusing a partial result"
        )));
    }
    Ok(ArchiveDeltaApplyOutcome::Applied(ArchiveDeltaApplyStats {
        missing_message_ids: expected,
        messages_applied: applied,
        projects_visited,
        duplicate_files_skipped: stats.duplicate_canonical_message_files,
    }))
}

/// Every `messages/YYYY/MM/*.md` file whose frontmatter id is in `wanted`.
fn collect_message_files_with_ids(
    messages_dir: &Path,
    wanted: &BTreeSet<i64>,
    out: &mut Vec<(i64, PathBuf)>,
) -> DbResult<()> {
    if !is_real_directory(messages_dir) {
        return Ok(());
    }
    let Ok(years) = std::fs::read_dir(messages_dir) else {
        return Ok(());
    };
    for year in years.flatten() {
        let year_path = year.path();
        if !is_real_directory(&year_path) {
            continue;
        }
        let Ok(months) = std::fs::read_dir(&year_path) else {
            continue;
        };
        for month in months.flatten() {
            let month_path = month.path();
            if !is_real_directory(&month_path) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(&month_path) else {
                continue;
            };
            for file in files.flatten() {
                let file_path = file.path();
                let Ok(file_type) = file.file_type() else {
                    continue;
                };
                if !file_type.is_file()
                    || file_type.is_symlink()
                    || file_path.extension().is_none_or(|e| e != "md")
                {
                    continue;
                }
                if let Some(id) = scan_archive_message_id(&file_path)?
                    && wanted.contains(&id)
                {
                    out.push((id, file_path));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact_test_directory_files(
        directory: &Path,
    ) -> std::collections::BTreeMap<std::ffi::OsString, Vec<u8>> {
        std::fs::read_dir(directory)
            .expect("read test directory")
            .map(|entry| {
                let entry = entry.expect("read test directory entry");
                let file_type = entry.file_type().expect("inspect test directory entry");
                assert!(
                    file_type.is_file(),
                    "test directory must contain only files"
                );
                (
                    entry.file_name(),
                    std::fs::read(entry.path()).expect("read exact test file bytes"),
                )
            })
            .collect()
    }

    /// Human-readable difference between two directory snapshots taken with
    /// [`exact_test_directory_files`]: which files appeared, vanished, or
    /// changed, with lengths and the first differing byte offset, so a
    /// byte-neutrality failure names the culprit instead of dumping bytes.
    fn describe_directory_diff(
        before: &std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>,
        after: &std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>,
    ) -> String {
        let mut lines = Vec::new();
        for (name, bytes) in before {
            match after.get(name) {
                None => lines.push(format!(
                    "removed {} ({} bytes)",
                    name.to_string_lossy(),
                    bytes.len()
                )),
                Some(after_bytes) if after_bytes != bytes => {
                    let first_diff = bytes
                        .iter()
                        .zip(after_bytes.iter())
                        .position(|(a, b)| a != b)
                        .unwrap_or_else(|| bytes.len().min(after_bytes.len()));
                    let window = |data: &[u8]| -> String {
                        data.iter()
                            .skip(first_diff)
                            .take(16)
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    };
                    lines.push(format!(
                        "changed {}: {} -> {} bytes, first difference at offset {first_diff} (before: [{}] after: [{}])",
                        name.to_string_lossy(),
                        bytes.len(),
                        after_bytes.len(),
                        window(bytes),
                        window(after_bytes)
                    ));
                }
                Some(_) => {}
            }
        }
        for (name, bytes) in after {
            if !before.contains_key(name) {
                lines.push(format!(
                    "added {} ({} bytes)",
                    name.to_string_lossy(),
                    bytes.len()
                ));
            }
        }
        if lines.is_empty() {
            "no differences".to_string()
        } else {
            lines.join("\n")
        }
    }

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
        assert_eq!(json_str_array(&v, "bcc"), [] as [String; 0]);
        assert_eq!(json_str_array(&v, "missing"), [] as [String; 0]);
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
        assert_eq!(cc_names, [] as [String; 0]);
        assert_eq!(bcc_names, [] as [String; 0]);
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
        assert_eq!(cc_names, [] as [String; 0]);
        assert_eq!(bcc_names, [] as [String; 0]);
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
        assert_eq!(cc_names, [] as [String; 0]);
        assert_eq!(bcc_names, [] as [String; 0]);
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
        assert_eq!(bcc_names, [] as [String; 0]);
        assert_eq!(stats.warnings, [] as [String; 0]);
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
            same_project_canonical_identity_collisions: 0,
            salvaged_projects: 0,
            salvaged_agents: 0,
            salvaged_messages: 0,
            salvaged_message_id_remaps: 0,
            salvaged_recipients: 0,
            salvaged_reservations: 0,
            salvaged_reservation_releases: 0,
            salvaged_rows_skipped_unmapped: 0,
            salvaged_placeholder_senders: 0,
            parse_errors: 3,
            warnings: vec![],
            suppressed_warnings: 0,
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

    #[cfg(unix)]
    #[test]
    fn fresh_reconstruction_claim_is_private_before_sqlite_opens_it() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let candidate = tmp.path().join("candidate.sqlite3");

        claim_fresh_reconstruction_target(&candidate).expect("claim private candidate");

        assert_eq!(
            std::fs::symlink_metadata(&candidate)
                .expect("candidate metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "candidate inode must be private before any mailbox bytes are written"
        );
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
            "retired_at": "2026-02-22T12:30:00Z",
            "deregistered_at": "2026-02-22T13:00:00Z",
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
        let lifecycle_rows = conn
            .query_sync(
                "SELECT a.retired_at, d.deregistered_at \
                 FROM agents a \
                 JOIN agent_deregistrations d ON d.agent_id = a.id \
                 WHERE a.name = 'TestAgent'",
                &[],
            )
            .expect("query reconstructed lifecycle state");
        assert_eq!(lifecycle_rows.len(), 1);
        assert_eq!(
            lifecycle_rows[0]
                .get_named::<i64>("retired_at")
                .expect("retired_at"),
            crate::iso_to_micros("2026-02-22T12:30:00Z").unwrap()
        );
        assert_eq!(
            lifecycle_rows[0]
                .get_named::<i64>("deregistered_at")
                .expect("deregistered_at"),
            crate::iso_to_micros("2026-02-22T13:00:00Z").unwrap()
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
    fn live_franken_salvage_materialization_preserves_wal_rows_and_source_bytes() {
        let source_dir = tempfile::tempdir().expect("source tempdir");
        let source_path = source_dir.path().join("live.sqlite3");
        let writer = crate::DbConn::open_file(source_path.to_string_lossy().as_ref())
            .expect("open live Franken salvage source");
        writer
            .execute_raw("PRAGMA journal_mode = WAL;")
            .expect("enable WAL mode");
        writer
            .execute_raw("PRAGMA wal_autocheckpoint = 0;")
            .expect("disable automatic WAL checkpoints");
        writer
            .execute_raw("CREATE TABLE salvage_witness(value INTEGER NOT NULL);")
            .expect("create salvage witness table");
        writer
            .execute_raw("INSERT INTO salvage_witness(value) VALUES (41);")
            .expect("commit WAL-only salvage witness");
        assert!(
            mcp_agent_mail_core::disk::sqlite_sidecar_path(&source_path, "-wal").is_file(),
            "fixture must retain a WAL sidecar"
        );
        assert!(
            mcp_agent_mail_core::disk::sqlite_sidecar_path(&source_path, "-shm").is_file(),
            "fixture must retain a SHM sidecar"
        );
        let source_before = exact_test_directory_files(source_dir.path());

        let materialized = materialize_live_salvage(&source_path)
            .expect("materialize guarded live salvage source");
        let private = open_read_only_salvage_db(&materialized.path)
            .expect("open private materialized salvage");
        let rows = private
            .query_sync("SELECT value FROM salvage_witness", &[])
            .expect("query WAL witness from private materialization");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<i64>("value").unwrap(), 41);
        drop(private);

        let source_after = exact_test_directory_files(source_dir.path());
        // The database file, the WAL, and every namespace sidecar must be
        // byte-identical. The `-shm` WAL-index is exempt in one region only:
        // SQLite's own reader protocol records the snapshot a reader is
        // using in the checkpoint-info block (`nBackfill` + `aReadMark[5]`,
        // bytes 96..116 of the header), and fsqlite 0.3.14's guarded
        // read-only open advances a read mark there exactly like the
        // canonical engine does (br-00gl8: observed as `aReadMark[1]`
        // 2 -> 3 at offset 104). The WAL-index is transient shared memory
        // that is rebuilt from the WAL whenever it is missing, so a moved
        // read mark changes no durable state.
        const SHM_CHECKPOINT_INFO: std::ops::Range<usize> = 96..116;
        let shm_name = std::ffi::OsString::from("live.sqlite3-shm");
        let mut durable_before = source_before.clone();
        let mut durable_after = source_after.clone();
        let shm_before = durable_before
            .remove(&shm_name)
            .expect("fixture keeps its SHM sidecar");
        let shm_after = durable_after
            .remove(&shm_name)
            .expect("guarded materialization must not delete the SHM sidecar");
        assert!(
            durable_after == durable_before,
            "guarded materialization must preserve every durable live source-family byte; differences:\n{}",
            describe_directory_diff(&durable_before, &durable_after)
        );
        assert_eq!(
            shm_after.len(),
            shm_before.len(),
            "SHM sidecar must keep its size"
        );
        let shm_differences: Vec<usize> = shm_before
            .iter()
            .zip(shm_after.iter())
            .enumerate()
            .filter(|(_, (before, after))| before != after)
            .map(|(offset, _)| offset)
            .collect();
        assert!(
            shm_differences
                .iter()
                .all(|offset| SHM_CHECKPOINT_INFO.contains(offset)),
            "SHM sidecar may only change inside the reader checkpoint-info block {SHM_CHECKPOINT_INFO:?}; changed offsets: {shm_differences:?}"
        );
        crate::close_db_conn(writer, "clean up WAL salvage materialization fixture");
    }

    #[test]
    fn live_franken_salvage_artifact_is_engine_neutral_and_passes_canonical_validation() {
        let source_dir = tempfile::tempdir().expect("source tempdir");
        let source_path = source_dir.path().join("live.sqlite3");
        let writer = crate::DbConn::open_file(source_path.to_string_lossy().as_ref())
            .expect("open live Franken salvage source");
        writer
            .execute_raw("PRAGMA journal_mode = WAL;")
            .expect("enable WAL mode");
        writer
            .execute_raw("CREATE TABLE salvage_witness(value INTEGER NOT NULL);")
            .expect("create salvage witness table");
        writer
            .execute_raw("INSERT INTO salvage_witness(value) VALUES (7);")
            .expect("commit salvage witness");

        let materialized = materialize_live_salvage(&source_path)
            .expect("materialize guarded live salvage source");

        for suffix in PRIVATE_SALVAGE_FRANKEN_NAMESPACE_SUFFIXES
            .iter()
            .chain(PRIVATE_SALVAGE_FRANKEN_WAL_WITNESS_SUFFIXES.iter())
            .chain(["-wal", "-shm"].iter())
        {
            let sidecar =
                mcp_agent_mail_core::disk::sqlite_sidecar_path(&materialized.path, suffix);
            assert!(
                std::fs::symlink_metadata(&sidecar).is_err(),
                "private salvage artifact must be a self-contained file without {suffix} residue: {}",
                sidecar.display()
            );
        }
        // The validator every downstream promotion path uses refuses any
        // Franken-admitted path; the private artifact must pass it.
        crate::pool::sqlite_file_passes_full_integrity_check(&materialized.path).expect(
            "engine-neutral private salvage artifact must pass the canonical full integrity check",
        );
        let private = open_read_only_salvage_db(&materialized.path)
            .expect("open private materialized salvage");
        let rows = private
            .query_sync("SELECT value FROM salvage_witness", &[])
            .expect("query witness from private materialization");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<i64>("value").unwrap(), 7);
        drop(private);
        crate::close_db_conn(writer, "clean up engine-neutral salvage fixture");
    }

    /// Seed a canonical-written source with no FrankenSQLite namespace pair:
    /// the shape of a primary restored from a backup or reconstructed from
    /// the archive that the archive has since outrun. Returns the source
    /// path and its byte-exact directory snapshot.
    fn seed_sidecarless_canonical_salvage_source(
        source_dir: &Path,
        schema_and_rows: &[&str],
    ) -> (
        PathBuf,
        std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>,
    ) {
        let source_path = source_dir.join("sidecarless.sqlite3");
        let source = SqliteDbConn::open_file(source_path.to_str().unwrap())
            .expect("open canonical sidecarless source");
        source
            .execute_raw("PRAGMA journal_mode = DELETE;")
            .expect("use standalone canonical source");
        for sql in schema_and_rows {
            source.execute_raw(sql).expect("seed canonical source");
        }
        drop(source);
        assert!(
            !mcp_agent_mail_core::disk::sqlite_sidecar_path(&source_path, "-fsqlite-ns-gate")
                .exists()
        );
        assert!(
            !mcp_agent_mail_core::disk::sqlite_sidecar_path(&source_path, "-fsqlite-ns-use")
                .exists()
        );
        let before = exact_test_directory_files(source_dir);
        (source_path, before)
    }

    /// br-vhxdc: a live source without a namespace pair is materialized
    /// through canonical SQLite (engine-dispatching read-only open), leaving
    /// the source family byte-identical, and its DB-only rows are merged
    /// into the reconstructed target. Before the dispatcher every such source
    /// was refused, which made archive-ahead recovery of a reconstructed or
    /// restored primary impossible.
    #[test]
    fn live_salvage_merges_sidecarless_canonical_source_into_reconstructed_target() {
        let source_dir = tempfile::tempdir().expect("source tempdir");
        let (source_path, source_before) = seed_sidecarless_canonical_salvage_source(
            source_dir.path(),
            &[
                "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT, created_at INTEGER);",
                "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL);",
                "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, subject TEXT, body_md TEXT, created_ts INTEGER);",
                "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL, read_ts INTEGER, ack_ts INTEGER);",
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (100, 'test-project', '/test-project', 1);",
                "INSERT INTO agents (id, project_id, name) VALUES (10, 100, 'Alice'), (11, 100, 'Bob');",
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts) VALUES (1, 100, 10, 'Archive copy', 'archive body', 1771761600000000), (2, 100, 10, 'DB-only', 'db body', 2);",
                "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 11, 'to', 123, 456), (2, 11, 'to', NULL, NULL);",
            ],
        );

        let archive_dir = tempfile::tempdir().expect("archive tempdir");
        let storage_root = archive_dir.path().join("storage");
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
            "---json\n{\"id\":1,\"from\":\"Alice\",\"to\":[\"Bob\"],\"subject\":\"Archive copy\",\"importance\":\"normal\",\"created_ts\":\"2026-02-22T12:00:00Z\"}\n---\n\narchive body\n",
        )
        .unwrap();
        let target_path = archive_dir.path().join("reconstructed.sqlite3");

        let stats =
            reconstruct_from_archive_with_live_salvage(&target_path, &storage_root, &source_path)
                .expect("a sidecar-less canonical source must be salvaged, not refused");
        assert_eq!(stats.messages, 1, "one message came from the archive");
        assert_eq!(
            stats.salvaged_messages, 1,
            "the DB-only message must be merged from the canonical-served source"
        );
        assert!(
            stats.warnings.is_empty(),
            "a healthy sidecar-less source must not degrade to archive-only: {:?}",
            stats.warnings
        );

        let target = SqliteDbConn::open_file(target_path.to_str().unwrap())
            .expect("open reconstructed target");
        let subjects = target
            .query_sync("SELECT subject FROM messages ORDER BY id", &[])
            .expect("query reconstructed messages")
            .into_iter()
            .map(|row| row.get_named::<String>("subject").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            subjects,
            vec!["Archive copy".to_string(), "DB-only".to_string()]
        );
        drop(target);

        assert_eq!(
            exact_test_directory_files(source_dir.path()),
            source_before,
            "canonical-served salvage must leave the source family byte-identical"
        );
    }

    /// The fail-closed half of the same contract: a sidecar-less source that
    /// canonical SQLite can open but whose mailbox schema is incomplete still
    /// refuses, and the refusal must not degrade into an archive-only target
    /// (which would silently drop DB-only coordination state) or touch the
    /// source family.
    #[test]
    fn live_salvage_of_sidecarless_source_with_incomplete_schema_fails_closed() {
        let source_dir = tempfile::tempdir().expect("source tempdir");
        let (source_path, source_before) = seed_sidecarless_canonical_salvage_source(
            source_dir.path(),
            &["CREATE TABLE messages(id INTEGER PRIMARY KEY);"],
        );

        let archive_dir = tempfile::tempdir().expect("archive tempdir");
        let storage_root = archive_dir.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects").join("archive-project"))
            .expect("create authoritative archive fixture");
        let target_path = archive_dir.path().join("reconstructed.sqlite3");
        let error =
            reconstruct_from_archive_with_live_salvage(&target_path, &storage_root, &source_path)
                .expect_err("an incomplete-schema live source must fail closed");
        let message = error.to_string();
        assert!(
            message.contains("messages schema is incomplete"),
            "the refusal must name the schema problem, not an engine refusal: {message}"
        );
        assert!(
            message.contains("DB-only coordination state could be lost"),
            "the refusal must say why the archive-only candidate was not promoted: {message}"
        );
        // Production callers hand this entry point a private candidate path
        // and promote it only on `Ok`; the archive-only candidate built before
        // the merge refused may remain at that path, but it is never reported
        // as a success. The source family must be exactly as it was.
        assert_eq!(
            exact_test_directory_files(source_dir.path()),
            source_before,
            "a salvage refusal must not create or rewrite source-family state"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_franken_salvage_materialization_preserves_same_process_writer_lock() {
        const CHILD_PATH_ENV: &str = "MCP_AGENT_MAIL_RECONSTRUCT_LOCK_PROBE_PATH";
        const CHILD_TEST_NAME: &str = "reconstruct::tests::live_franken_salvage_materialization_preserves_same_process_writer_lock";
        const CHILD_WITNESS: &str = "live-salvage-child-observed-busy";

        if let Some(path) = std::env::var_os(CHILD_PATH_ENV) {
            // Competing writer through the runtime engine (FrankenSQLite): canonical
            // SQLite is never a mailbox writer and is not excluded by FrankenSQLite's
            // namespace/WAL-certificate coordination (br-0dw2c). The watchdog turns a
            // blocking engine into a visible failure instead of a hung suite.
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(15));
                eprintln!("child lock probe watchdog: competing write did not fail within 15 s");
                std::process::exit(3);
            });
            let competitor =
                crate::DbConn::open_file(PathBuf::from(path).to_string_lossy().as_ref())
                    .expect("child opens competing runtime-engine connection");
            let _ = competitor.execute_raw("PRAGMA busy_timeout = 10;");
            competitor
                .query_sync("SELECT value FROM salvage_witness", &[])
                .expect("child proves it opened the intended readable database");
            assert!(
                competitor.execute_raw("BEGIN IMMEDIATE;").is_err(),
                "separate process must not acquire the parent's reserved writer lock"
            );
            println!("{CHILD_WITNESS}");
            return;
        }

        fn assert_child_observes_busy(path: &Path) {
            let output = std::process::Command::new(
                std::env::current_exe().expect("resolve current test executable"),
            )
            .arg(CHILD_TEST_NAME)
            .arg("--exact")
            .arg("--nocapture")
            .env(CHILD_PATH_ENV, path)
            .output()
            .expect("run competing child lock probe");
            assert!(
                output.status.success(),
                "child lock probe failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(CHILD_WITNESS),
                "child filter ran no causal lock probe: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let directory = tempfile::tempdir().expect("tempdir");
        let source_path = directory.path().join("writer-lock.sqlite3");
        let storage_root = directory.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects").join("lock-project"))
            .expect("create lock-test archive fixture");
        let writer = crate::DbConn::open_file(source_path.to_string_lossy().as_ref())
            .expect("open Franken writer-lock fixture");
        writer
            .execute_raw("PRAGMA journal_mode = DELETE;")
            .expect("use rollback-journal locking");
        writer
            .execute_raw("CREATE TABLE salvage_witness(value INTEGER NOT NULL);")
            .expect("create writer-lock witness table");
        writer
            .execute_raw("INSERT INTO salvage_witness(value) VALUES (73);")
            .expect("insert writer-lock witness");
        writer
            .execute_raw("BEGIN IMMEDIATE;")
            .expect("acquire parent reserved writer lock");

        assert_child_observes_busy(&source_path);
        let materialized = materialize_live_salvage(&source_path)
            .expect("materialize live source without touching parent writer lock");
        let private = open_read_only_salvage_db(&materialized.path)
            .expect("open private materialized salvage");
        assert_eq!(
            private
                .query_sync("SELECT value FROM salvage_witness", &[])
                .expect("query private writer-lock witness")[0]
                .get_named::<i64>("value")
                .unwrap(),
            73
        );
        drop(private);
        assert_child_observes_busy(&source_path);

        let alias_path = directory.path().join("writer-lock-alias.sqlite3");
        std::fs::hard_link(&source_path, &alias_path).expect("create live-primary hard-link alias");
        let alias_target = directory.path().join("alias-reconstructed.sqlite3");
        let alias_error = reconstruct_from_archive_with_private_salvage(
            &alias_target,
            &storage_root,
            &alias_path,
        )
        .expect_err("hard-link alias cannot be classified as private salvage");
        assert!(
            alias_error.to_string().contains("hard links"),
            "unexpected hard-link refusal: {alias_error}"
        );
        assert!(
            !alias_target.exists(),
            "hard-link refusal must happen before reconstruction"
        );
        assert_child_observes_busy(&source_path);

        writer
            .execute_raw("ROLLBACK;")
            .expect("release parent writer lock");
        crate::close_db_conn(writer, "clean up salvage writer-lock fixture");
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

    /// Regression for mcp_agent_mail_rust#219: writes racing a recovery
    /// promotion left `agent_links` / `messages` / `message_recipients` rows
    /// referencing agent ids that never existed in the salvage source. The
    /// strict merge refused the entire recovery ("unmapped origin agent
    /// 925") forever, wedging the mailbox until manual DB surgery. Dangling
    /// coordination rows must degrade to itemized skips, and unmapped
    /// senders must degrade to placeholder agents so message content
    /// survives.
    #[test]
    fn salvage_merge_survives_dangling_cross_generation_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed.db");
        let salvage_db_path = tmp.path().join("salvage.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT, created_at INTEGER)",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT, model TEXT, task_description TEXT, inception_ts INTEGER, last_active_ts INTEGER)",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, thread_id TEXT, subject TEXT, body_md TEXT, importance TEXT, ack_required INTEGER, created_ts INTEGER, recipients_json TEXT, attachments TEXT)",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL, read_ts INTEGER, ack_ts INTEGER)",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agent_links (id INTEGER PRIMARY KEY, a_project_id INTEGER NOT NULL, a_agent_id INTEGER NOT NULL, b_project_id INTEGER NOT NULL, b_agent_id INTEGER NOT NULL, status TEXT, reason TEXT, created_ts INTEGER, updated_ts INTEGER, expires_ts INTEGER)",
            )
            .unwrap();

        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (20, 'test-project', '/test-project', 1)",
                &[],
            )
            .unwrap();
        // Two live identities. Ids 925, 1240, and 464 are deliberately
        // absent from `agents` — exactly the incident shape.
        salvage_conn
            .query_sync(
                "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts) \
                 VALUES (5, 20, 'RealAgent', 'codex', 'gpt', '', 10, 10), (6, 20, 'PeerAgent', 'codex', 'gpt', '', 10, 10)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) VALUES \
                 (42, 20, 1240, NULL, 'orphan sender', 'body from the racing write', 'normal', 0, 100, '{}', '[]'), \
                 (43, 20, 5, NULL, 'healthy', 'body', 'normal', 0, 101, '{}', '[]')",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) \
                 VALUES (43, 464, 'to', NULL, NULL), (43, 6, 'to', 5, NULL)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agent_links (id, a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts, expires_ts) VALUES \
                 (507, 20, 925, 20, 6, 'approved', 'auto-handshake by send_message', 100, 101, NULL), \
                 (508, 20, 5, 20, 6, 'approved', 'auto-handshake by send_message', 100, 101, NULL)",
                &[],
            )
            .unwrap();
        drop(salvage_conn);

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("merge must survive dangling cross-generation rows");

        assert_eq!(
            stats.salvaged_placeholder_senders, 1,
            "sender 1240 must degrade to a placeholder agent"
        );
        assert!(
            stats.salvaged_rows_skipped_unmapped >= 2,
            "dangling link (agent 925) and recipient (agent 464) must be skipped, got {}",
            stats.salvaged_rows_skipped_unmapped
        );
        assert!(
            stats
                .warnings
                .iter()
                .any(|w| w.contains("agent_link") && w.contains("dangling")),
            "the skipped link must be itemized: {:?}",
            stats.warnings
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        // The racing write's message content survived, re-homed under the
        // placeholder sender.
        let rows = conn
            .query_sync(
                "SELECT a.name AS name, a.program AS program FROM messages m JOIN agents a ON a.id = m.sender_id WHERE m.id = 42",
                &[],
            )
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "message 42 must be carried into the candidate"
        );
        assert_eq!(
            rows[0].get_named::<String>("name").unwrap(),
            "unknown-agent-1240"
        );
        assert_eq!(rows[0].get_named::<String>("program").unwrap(), "unknown");
        // The healthy link survived; the dangling one did not.
        let link_rows = conn
            .query_sync("SELECT COUNT(*) AS n FROM agent_links", &[])
            .unwrap();
        assert_eq!(link_rows[0].get_named::<i64>("n").unwrap(), 1);
        // Healthy recipient state survived; the dangling recipient did not.
        let recip = conn
            .query_sync(
                "SELECT agent_id FROM message_recipients WHERE message_id = 43",
                &[],
            )
            .unwrap();
        assert_eq!(recip.len(), 1, "only the mappable recipient row survives");
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
                 INSERT INTO agents \
                     (id, project_id, name, program, model, task_description, \
                      inception_ts, last_active_ts, attachments_policy, contact_policy) \
                     VALUES \
                     (7, 1, 'CanonicalAgent', 'codex-cli', 'test', '', 1, 2, 'auto', 'auto');",
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
  "topic": "br-abc.1",
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
                "SELECT subject, body_md, thread_id, topic, recipients_json FROM messages LIMIT 1",
                &[],
            )
            .unwrap();
        assert!(!rows.is_empty(), "message should exist in DB");
        assert_eq!(
            rows[0].get_named::<String>("topic").expect("topic"),
            "br-abc.1"
        );
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
        // A drifted duplicate ARTIFACT of the same logical message: identical
        // identity (created_ts + subject), divergent volatile payload
        // (recipients/importance/body). Only identity-identical artifacts are
        // duplicates — a same-id file with a different identity is row-id
        // reuse and is preserved instead (br-r6awv, covered separately by
        // `reconstruct_preserves_same_project_canonical_id_reuse_with_distinct_identity`).
        std::fs::write(
            messages_dir.join("2026-02-22T12-01-00Z__second__7.md"),
            r#"---json
{
  "id": 7,
  "from": "Alice",
  "to": ["Carol"],
  "subject": "First copy",
  "importance": "urgent",
  "created_ts": "2026-02-22T12:00:00Z"
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
    fn reconstruct_skips_deferred_collision_losers_that_share_an_identity() {
        // Three artifacts share canonical id 7: the winner (identity X) plus
        // two losers that are copies of identity Y. Pass 1 keeps X and queues
        // both Y files. Pass 2 must insert Y once, not twice.
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
            messages_dir.join("2026-02-22T12-00-00Z__winner__7.md"),
            r#"---json
{
  "id": 7,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "Winner",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

winner body
"#,
        )
        .unwrap();
        for name in [
            "2026-02-22T12-01-00Z__loser-a__7.md",
            "2026-02-22T12-02-00Z__loser-b__7.md",
        ] {
            std::fs::write(
                messages_dir.join(name),
                r#"---json
{
  "id": 7,
  "from": "Alice",
  "to": ["Carol"],
  "subject": "Loser",
  "created_ts": "2026-02-22T12:01:00Z"
}
---

loser body
"#,
            )
            .unwrap();
        }

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.messages, 2, "winner plus one deferred loser");
        assert_eq!(stats.duplicate_canonical_message_files, 1);
        assert!(
            stats.same_project_canonical_identity_collisions >= 1,
            "both losers must be recognized as id-7 identity collisions, got {}",
            stats.same_project_canonical_identity_collisions
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let subject_rows = conn
            .query_sync(
                "SELECT subject, COUNT(*) AS cnt FROM messages GROUP BY subject ORDER BY subject",
                &[],
            )
            .unwrap();
        assert_eq!(subject_rows.len(), 2);
        let counts: Vec<(String, i64)> = subject_rows
            .iter()
            .map(|row| {
                (
                    row.get_named::<String>("subject").expect("subject"),
                    row.get_named::<i64>("cnt").expect("cnt"),
                )
            })
            .collect();
        assert_eq!(
            counts,
            vec![("Loser".to_string(), 1), ("Winner".to_string(), 1)]
        );
    }

    #[test]
    fn canonical_filename_created_ts_parses_leading_utc_stamp() {
        let path = Path::new("/archive/2026-02-22T12-00-00Z__hello__1.md");
        assert_eq!(
            super::canonical_filename_created_ts(path),
            Some(1_771_761_600_000_000)
        );
        assert_eq!(
            super::canonical_filename_created_ts(Path::new("no-stamp.md")),
            None
        );
        assert_eq!(
            super::canonical_filename_created_ts(Path::new("hello__7.md")),
            None
        );
    }

    #[test]
    fn salvage_ts_or_zero_keeps_decoded_values_and_falls_back_to_zero() {
        assert_eq!(super::salvage_ts_or_zero::<()>(Ok(42)), 42);
        assert_eq!(super::salvage_ts_or_zero::<&str>(Err("missing")), 0);
    }

    #[test]
    fn reconstruct_uses_filename_stamp_when_frontmatter_has_no_created_ts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");
        let project_dir = storage_root.join("projects").join("stamp-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"stamp-project","human_key":"/stamp-project","created_at":0}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-02-22T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__no-ts__3.md"),
            r#"---json
{
  "id": 3,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "No ts"
}
---

body
"#,
        )
        .unwrap();

        reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT created_ts FROM messages WHERE id = 3", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<i64>("created_ts").expect("created_ts"),
            1_771_761_600_000_000,
            "missing frontmatter created_ts must fall back to the filename stamp, not now_micros()"
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
    fn reconstruct_collision_generated_id_never_displaces_later_canonical_id() {
        // br-r6awv reproduction: project "a-junk" (walked first) claims
        // canonical id 1. Project "b-real" holds a *different* message with
        // canonical id 1 plus a later message with canonical id 2. Prior
        // behavior inserted the displaced id-1 message immediately under the
        // next generated rowid — which was 2 — so the real canonical __2.md
        // file was then misclassified as a same-project duplicate and
        // silently dropped. Expected: all three messages survive.
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let write_project = |slug: &str| {
            let project_dir = storage_root.join("projects").join(slug);
            std::fs::create_dir_all(project_dir.join("messages").join("2026").join("02")).unwrap();
            std::fs::write(
                project_dir.join("project.json"),
                format!(r#"{{"slug":"{slug}","human_key":"/{slug}","created_at":0}}"#),
            )
            .unwrap();
            project_dir
        };
        let junk_dir = write_project("a-junk");
        let real_dir = write_project("b-real");
        let write_message = |dir: &std::path::Path,
                             name: &str,
                             id: i64,
                             subject: &str,
                             created: &str| {
            std::fs::write(
                    dir.join("messages").join("2026").join("02").join(name),
                    format!(
                        "---json\n{{\"id\":{id},\"from\":\"Alice\",\"to\":[\"Bob\"],\"subject\":\"{subject}\",\"importance\":\"normal\",\"created_ts\":\"{created}\"}}\n---\n\nbody\n"
                    ),
                )
                .unwrap();
        };
        write_message(
            &junk_dir,
            "2026-02-01T00-00-00Z__junk__1.md",
            1,
            "Junk one",
            "2026-02-01T00:00:00Z",
        );
        write_message(
            &real_dir,
            "2026-02-10T00-00-00Z__real-one__1.md",
            1,
            "Real one",
            "2026-02-10T00:00:00Z",
        );
        write_message(
            &real_dir,
            "2026-02-10T00-00-05Z__real-two__2.md",
            2,
            "Real two",
            "2026-02-10T00:00:05Z",
        );

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(
            stats.messages, 3,
            "all three messages must survive; warnings: {:?}",
            stats.warnings
        );
        assert_eq!(stats.cross_project_canonical_collisions, 1);
        assert_eq!(
            stats.duplicate_canonical_message_files, 0,
            "no real message may be dropped as a duplicate"
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT id, subject FROM messages ORDER BY id", &[])
            .unwrap();
        let pairs: Vec<(i64, String)> = rows
            .iter()
            .map(|r| {
                (
                    r.get_named::<i64>("id").unwrap(),
                    r.get_named::<String>("subject").unwrap(),
                )
            })
            .collect();
        // Canonical ids 1 and 2 hold the first-visited claimants; the
        // displaced message lands on a generated id ABOVE every canonical id.
        assert_eq!(pairs[0], (1, "Junk one".to_string()));
        assert_eq!(pairs[1], (2, "Real two".to_string()));
        assert_eq!(pairs[2].1, "Real one");
        assert!(
            pairs[2].0 > 2,
            "displaced message must get a generated id above all canonical ids, got {}",
            pairs[2].0
        );
    }

    #[test]
    fn reconstruct_preserves_same_project_canonical_id_reuse_with_distinct_identity() {
        // One project, three artifacts claiming canonical id 5:
        //   - fileA: identity X (kept, canonical id 5)
        //   - fileB: identity X again (true duplicate artifact — skipped)
        //   - fileC: identity Y (row-id reuse across DB generations —
        //     preserved under a generated id, NOT dropped)
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");
        let project_dir = storage_root.join("projects").join("solo");
        let messages_dir = project_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"solo","human_key":"/solo","created_at":0}"#,
        )
        .unwrap();
        let write_message = |name: &str, subject: &str, created: &str| {
            std::fs::write(
                messages_dir.join(name),
                format!(
                    "---json\n{{\"id\":5,\"from\":\"Alice\",\"to\":[\"Bob\"],\"subject\":\"{subject}\",\"importance\":\"normal\",\"created_ts\":\"{created}\"}}\n---\n\nbody\n"
                ),
            )
            .unwrap();
        };
        write_message(
            "2026-03-01T00-00-00Z__gen-one__5.md",
            "Gen one",
            "2026-03-01T00:00:00Z",
        );
        write_message(
            "2026-03-01T00-00-00Z__gen-one-copy__5.md",
            "Gen one",
            "2026-03-01T00:00:00Z",
        );
        write_message(
            "2026-03-09T00-00-00Z__gen-two__5.md",
            "Gen two",
            "2026-03-09T00:00:00Z",
        );

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(
            stats.messages, 2,
            "distinct identities must both survive; warnings: {:?}",
            stats.warnings
        );
        assert_eq!(stats.duplicate_canonical_message_files, 1);
        assert_eq!(stats.same_project_canonical_identity_collisions, 1);
        assert!(
            stats
                .warnings
                .iter()
                .any(|w| w.contains("row-id reuse across DB generations")),
            "expected same-project reuse warning, got {:?}",
            stats.warnings
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT id, subject FROM messages ORDER BY id", &[])
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get_named::<i64>("id").unwrap(), 5);
        assert_eq!(rows[0].get_named::<String>("subject").unwrap(), "Gen one");
        assert!(rows[1].get_named::<i64>("id").unwrap() > 5);
        assert_eq!(rows[1].get_named::<String>("subject").unwrap(), "Gen two");
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

    #[test]
    fn reconstruct_reservation_missing_created_ts_is_deterministic() {
        // Reservation identity includes created_ts. A now_micros() fallback
        // would mint a new lease identity on every recovery and refuse
        // promotion (br-r6awv). Two archive-only recoveries of the same
        // artifact must land the same created_ts.
        let storage_root = tempfile::tempdir().expect("tempdir");
        let project_dir = storage_root
            .path()
            .join("projects")
            .join("legacy-reservation");
        let agents_dir = project_dir.join("agents").join("CoralMarsh");
        let reservations_dir = project_dir.join("file_reservations");
        std::fs::create_dir_all(&agents_dir).expect("create agents dir");
        std::fs::create_dir_all(&reservations_dir).expect("create reservations dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"legacy-reservation","human_key":"/legacy-reservation","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agents_dir.join("profile.json"),
            r#"{"name":"CoralMarsh","program":"codex-cli","model":"gpt-5"}"#,
        )
        .expect("write agent profile");
        std::fs::write(
            reservations_dir.join("id-42.json"),
            r#"{
                "id": 42,
                "project": "/legacy-reservation",
                "agent": "CoralMarsh",
                "path_pattern": "src/**",
                "exclusive": true,
                "reason": "legacy-no-created-ts"
            }"#,
        )
        .expect("write reservation without created_ts");

        let mut created_ats = Vec::new();
        for idx in 0..2 {
            let db_dir = tempfile::tempdir().expect("tempdir");
            let db_path = db_dir.path().join(format!("legacy_res_{idx}.sqlite3"));
            reconstruct_from_archive(&db_path, storage_root.path()).expect("reconstruct");
            let conn = SqliteDbConn::open_file(db_path.display().to_string()).expect("open db");
            let rows = conn
                .query_sync(
                    "SELECT CAST(created_ts AS INTEGER) AS created_ts, CAST(expires_ts AS INTEGER) AS expires_ts \
                     FROM file_reservations ORDER BY id",
                    &[],
                )
                .expect("query reservation timestamps");
            assert_eq!(rows.len(), 1, "exactly one reservation must be recovered");
            created_ats.push(rows[0].get_named::<i64>("created_ts").unwrap());
            assert_eq!(
                rows[0].get_named::<i64>("expires_ts").unwrap(),
                created_ats[idx],
                "missing expires_ts must follow the deterministic created_ts"
            );
        }
        assert_eq!(
            created_ats[0], 0,
            "missing reservation created_ts must fall back to 0, not now_micros()"
        );
        assert_eq!(
            created_ats[0], created_ats[1],
            "two recoveries of the same untimestamped reservation must not mint a new identity"
        );
    }

    #[test]
    fn salvage_does_not_duplicate_a_lease_the_archive_already_merged_across_generations() {
        // Live outage 2026-08-28 (hfdt-agent-mail-reservation-parity-drift-o3902):
        // `am doctor reconstruct` rebuilt 44 agents / 3131 messages and then
        // REFUSED promotion with "reservations produced 881 rows but only 873
        // unique stable keys; refusing ambiguous recovery".
        //
        // Measured on the real failed candidate: all 8 collisions were 2 rows with
        // the SAME project_id, SAME agent_id, and identical path / exclusive /
        // reason / created_ts / expires_ts / released_ts, differing only in `id`.
        // In the archive each was ONE reservation id present under BOTH database
        // generations, active in the older and released in the newer:
        //   id-174-g2c9ef98....json  released_ts = null
        //   id-174-g90bc42e0....json released_ts = 2026-08-28T03:02:27.101444Z
        //
        // The archive-only path already collapses that pair (proved by
        // `reconstruct_merges_reservation_lifecycle_artifacts_across_generations`).
        // The production path also merges the unhealthy live DB, and THAT is the
        // uncovered combination: the salvaged row must land on the row the archive
        // replay already merged, not become a phantom fresh-id duplicate.
        //
        // This is the negative control for the stable-key collision: two rows here
        // is exactly what makes `require_unique_receipt_keys` refuse recovery.
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed.sqlite3");
        let salvage_db_path = tmp.path().join("salvage.sqlite3");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("outage-project");
        let agents_dir = project_dir.join("agents").join("TanMink");
        let reservations_dir = project_dir.join("file_reservations");
        std::fs::create_dir_all(&agents_dir).expect("agents dir");
        std::fs::create_dir_all(&reservations_dir).expect("reservations dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"outage-project","human_key":"/outage-project","created_at":0}"#,
        )
        .expect("project meta");
        std::fs::write(
            agents_dir.join("profile.json"),
            r#"{"name":"TanMink","program":"codex-cli","model":"gpt-5","task_description":"","inception_ts":"2026-08-27T05:00:00Z","last_active_ts":"2026-08-27T05:00:00Z"}"#,
        )
        .expect("agent profile");

        // The real archive shape: one id, two generations, active then released.
        let created = "2026-08-27T06:22:00.659794Z";
        let expires = "2026-08-27T07:52:00.659794Z";
        let released = "2026-08-28T03:02:27.101444Z";
        let active = format!(
            r#"{{"id":174,"db_generation":"2c9ef981","project":"/outage-project","agent":"TanMink","path_pattern":"scripts/release-build.sh","exclusive":true,"reason":"release lane","created_ts":"{created}","expires_ts":"{expires}","released_ts":null}}"#
        );
        let released_artifact = format!(
            r#"{{"id":174,"db_generation":"90bc42e0","project":"/outage-project","agent":"TanMink","path_pattern":"scripts/release-build.sh","exclusive":true,"reason":"release lane","created_ts":"{created}","expires_ts":"{expires}","released_ts":"{released}"}}"#
        );
        std::fs::write(reservations_dir.join("id-174-g2c9ef981.json"), &active)
            .expect("active artifact");
        std::fs::write(
            reservations_dir.join("id-174-g90bc42e0.json"),
            &released_artifact,
        )
        .expect("released artifact");

        let created_us = chrono::DateTime::parse_from_rfc3339(created)
            .unwrap()
            .timestamp_micros();
        let expires_us = chrono::DateTime::parse_from_rfc3339(expires)
            .unwrap()
            .timestamp_micros();
        let released_us = chrono::DateTime::parse_from_rfc3339(released)
            .unwrap()
            .timestamp_micros();

        // The unhealthy live DB still carries the same lease under its own local
        // id. Deliberately make that id differ from the archive's 174: numeric ids
        // are database-local, so using 174 here would let an accidental id-based
        // merge pass without proving the stable reservation identity is honored.
        let salvage_conn =
            SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).expect("salvage db");
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT, created_at INTEGER)",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT, model TEXT, task_description TEXT, inception_ts INTEGER, last_active_ts INTEGER)",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL, reason TEXT NOT NULL, created_ts INTEGER NOT NULL, expires_ts INTEGER NOT NULL, released_ts INTEGER)",
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (20, 'outage-project', '/outage-project', 1)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES (5, 20, 'TanMink', 'codex-cli', 'gpt-5', '', 10, 10)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
                 VALUES (?, 20, 5, 'scripts/release-build.sh', 1, 'release lane', ?, ?, ?)",
                &[
                    Value::BigInt(9_174),
                    Value::BigInt(created_us),
                    Value::BigInt(expires_us),
                    Value::BigInt(released_us),
                ],
            )
            .unwrap();
        drop(salvage_conn);

        reconstruct_from_archive_with_private_salvage(
            &db_path,
            storage_root.as_path(),
            &salvage_db_path,
        )
        .expect("reconstruct with salvage");

        let conn = SqliteDbConn::open_file(db_path.display().to_string()).expect("open db");
        let rows = conn
            .query_sync(
                "SELECT id, path_pattern, created_ts, released_ts FROM file_reservations ORDER BY id ASC",
                &[],
            )
            .expect("query reservations");
        assert_eq!(
            rows.len(),
            1,
            "one lease across two archive generations plus the live salvage row must \
             import exactly once; duplicates collide on the promotion receipt's stable \
             key and refuse recovery (got ids {:?})",
            rows.iter()
                .map(|row| row.get_named::<i64>("id").unwrap_or(-1))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            rows[0].get_named::<i64>("id").unwrap(),
            174,
            "the archive's canonical reservation id must remain authoritative over the \
             salvage database's unrelated local id"
        );
        assert_eq!(
            rows[0].get_named::<Option<i64>>("released_ts").unwrap(),
            Some(released_us),
            "the released generation must win over the active one"
        );
    }

    #[test]
    fn reconstruct_preserves_cross_generation_reservations_with_reused_id() {
        // br-n8qh6: two DB generations wrote a reservation with the SAME global id
        // 1 to the same archive. The generation-stamped filenames keep both
        // artifacts on disk, and reconstruct must recover BOTH rows (one at the
        // preserved id, one re-keyed under a fresh id) instead of collapsing them
        // to one via INSERT OR REPLACE — the silent-loss the bead reports.
        let storage_root = tempfile::tempdir().expect("tempdir");
        let db_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = storage_root.path().join("projects").join("gen-project");
        let agents_dir = project_dir.join("agents").join("CoralMarsh");
        let reservations_dir = project_dir.join("file_reservations");
        std::fs::create_dir_all(&agents_dir).expect("agents dir");
        std::fs::create_dir_all(&reservations_dir).expect("reservations dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"gen-project","human_key":"/gen-project","created_at":0}"#,
        )
        .expect("project meta");
        std::fs::write(
            agents_dir.join("profile.json"),
            r#"{"name":"CoralMarsh","program":"codex-cli","model":"gpt-5","task_description":"","inception_ts":"2026-03-13T21:21:02Z","last_active_ts":"2026-03-13T21:21:02Z"}"#,
        )
        .expect("agent profile");

        let gen1 = r#"{"id":1,"db_generation":"aaaa1111","project":"/gen-project","agent":"CoralMarsh","path_pattern":"src/first.rs","exclusive":true,"reason":"gen1","created_ts":"2026-03-13T21:36:47.221175Z","expires_ts":"2026-03-13T23:36:47.221175Z"}"#;
        let gen2 = r#"{"id":1,"db_generation":"bbbb2222","project":"/gen-project","agent":"CoralMarsh","path_pattern":"src/second.rs","exclusive":true,"reason":"gen2","created_ts":"2026-03-13T21:36:47.221175Z","expires_ts":"2026-03-13T23:36:47.221175Z"}"#;
        std::fs::write(reservations_dir.join("id-1-gaaaa1111.json"), gen1).expect("gen1");
        std::fs::write(reservations_dir.join("id-1-gbbbb2222.json"), gen2).expect("gen2");

        let db_path = db_dir.path().join("gen.sqlite3");
        reconstruct_from_archive(&db_path, storage_root.path()).expect("reconstruct");

        let conn = SqliteDbConn::open_file(db_path.display().to_string()).expect("open db");
        let rows = conn
            .query_sync(
                "SELECT path_pattern FROM file_reservations ORDER BY path_pattern ASC",
                &[],
            )
            .expect("query reservations");
        assert_eq!(
            rows.len(),
            2,
            "both generations' reservations must survive reconstruct (no id collapse)"
        );
        assert_eq!(
            rows[0].get_named::<String>("path_pattern").unwrap(),
            "src/first.rs"
        );
        assert_eq!(
            rows[1].get_named::<String>("path_pattern").unwrap(),
            "src/second.rs"
        );
    }

    #[test]
    fn reconstruct_dedups_identical_reservation_across_legacy_and_generation_artifacts() {
        // Observed live 2026-08-15 (br-gotf0): ONE reservation surfaced under both
        // the legacy `id-<id>.json` name and the generation-stamped
        // `id-<id>-g<gen>.json` name. The (project, generation, id) dedup treats
        // those as distinct artifacts and the id-collision fallback re-inserts the
        // second copy under a fresh id — byte-identical duplicate rows that the
        // promotion receipt's stable-key collision check rightly refuses
        // (14802 rows / 14797 unique keys wedged the production repair). Identical
        // identity = one reservation = one row.
        let storage_root = tempfile::tempdir().expect("tempdir");
        let db_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = storage_root.path().join("projects").join("dup-project");
        let agents_dir = project_dir.join("agents").join("CoralMarsh");
        let reservations_dir = project_dir.join("file_reservations");
        std::fs::create_dir_all(&agents_dir).expect("agents dir");
        std::fs::create_dir_all(&reservations_dir).expect("reservations dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"dup-project","human_key":"/dup-project","created_at":0}"#,
        )
        .expect("project meta");
        std::fs::write(
            agents_dir.join("profile.json"),
            r#"{"name":"CoralMarsh","program":"codex-cli","model":"gpt-5","task_description":"","inception_ts":"2026-03-13T21:21:02Z","last_active_ts":"2026-03-13T21:21:02Z"}"#,
        )
        .expect("agent profile");

        // Same reservation content; only the artifact naming differs (and the
        // generation-stamped copy carries db_generation).
        let legacy = r#"{"id":7,"project":"/dup-project","agent":"CoralMarsh","path_pattern":"src/same.rs","exclusive":true,"reason":"same lease","created_ts":"2026-03-13T21:36:47.221175Z","expires_ts":"2026-03-13T23:36:47.221175Z"}"#;
        let stamped = r#"{"id":7,"db_generation":"cccc3333","project":"/dup-project","agent":"CoralMarsh","path_pattern":"src/same.rs","exclusive":true,"reason":"same lease","created_ts":"2026-03-13T21:36:47.221175Z","expires_ts":"2026-03-13T23:36:47.221175Z"}"#;
        std::fs::write(reservations_dir.join("id-7.json"), legacy).expect("legacy artifact");
        std::fs::write(reservations_dir.join("id-7-gcccc3333.json"), stamped)
            .expect("stamped artifact");

        let db_path = db_dir.path().join("dup.sqlite3");
        reconstruct_from_archive(&db_path, storage_root.path()).expect("reconstruct");

        let conn = SqliteDbConn::open_file(db_path.display().to_string()).expect("open db");
        let rows = conn
            .query_sync("SELECT path_pattern FROM file_reservations", &[])
            .expect("query reservations");
        assert_eq!(
            rows.len(),
            1,
            "identical reservation identity across legacy and generation artifacts \
             must import exactly once (stable-key collision otherwise refuses promotion)"
        );
    }

    #[test]
    fn reconstruct_merges_reservation_lifecycle_artifacts_across_generations() {
        // br-1texm: an older archive generation can show an active lease while a
        // later generation shows that same lease released. Reconstructing both as
        // rows lets salvage apply the release to the active copy and creates an
        // exact stable-key collision under a phantom fresh id.
        let storage_root = tempfile::tempdir().expect("tempdir");
        let db_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = storage_root
            .path()
            .join("projects")
            .join("lifecycle-project");
        let agents_dir = project_dir.join("agents").join("CoralMarsh");
        let reservations_dir = project_dir.join("file_reservations");
        std::fs::create_dir_all(&agents_dir).expect("agents dir");
        std::fs::create_dir_all(&reservations_dir).expect("reservations dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"lifecycle-project","human_key":"/lifecycle-project","created_at":0}"#,
        )
        .expect("project meta");
        std::fs::write(
            agents_dir.join("profile.json"),
            r#"{"name":"CoralMarsh","program":"codex-cli","model":"gpt-5","task_description":"","inception_ts":"2026-03-13T21:21:02Z","last_active_ts":"2026-03-13T21:21:02Z"}"#,
        )
        .expect("agent profile");

        // Sort the released artifact first to prove that a later-read stale active
        // generation cannot regress the terminal lifecycle state.
        let released = r#"{"id":7,"db_generation":"aaaa1111","project":"/lifecycle-project","agent":"CoralMarsh","path_pattern":"src/same.rs","exclusive":true,"reason":"same lease","created_ts":"2026-03-13T21:36:47.221175Z","expires_ts":"2026-03-13T23:36:47.221175Z","released_ts":"2026-03-13T22:00:00Z"}"#;
        let active = r#"{"id":7,"db_generation":"bbbb2222","project":"/lifecycle-project","agent":"CoralMarsh","path_pattern":"src/same.rs","exclusive":true,"reason":"same lease","created_ts":"2026-03-13T21:36:47.221175Z","expires_ts":"2026-03-13T23:36:47.221175Z","released_ts":null}"#;
        std::fs::write(reservations_dir.join("id-7-gaaaa1111.json"), released)
            .expect("released artifact");
        std::fs::write(reservations_dir.join("id-7-gbbbb2222.json"), active)
            .expect("active artifact");

        let db_path = db_dir.path().join("lifecycle.sqlite3");
        reconstruct_from_archive(&db_path, storage_root.path()).expect("reconstruct");

        let conn = SqliteDbConn::open_file(db_path.display().to_string()).expect("open db");
        let rows = conn
            .query_sync(
                "SELECT id, released_ts FROM file_reservations ORDER BY id ASC",
                &[],
            )
            .expect("query reservations");
        assert_eq!(rows.len(), 1, "one stable lease must produce one row");
        assert_eq!(rows[0].get_named::<i64>("id").unwrap(), 7);
        let expected_release = chrono::DateTime::parse_from_rfc3339("2026-03-13T22:00:00Z")
            .unwrap()
            .timestamp_micros();
        assert_eq!(
            rows[0].get_named::<Option<i64>>("released_ts").unwrap(),
            Some(expected_release),
            "released state must outrank an active artifact for the same lease"
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
                // The archive copy of a message carries the DB's exact
                // created_ts (µs), so a salvage row for an already-carried
                // message matches the candidate's identity precisely; a
                // same-id row whose identity differs is row-id reuse and gets
                // remapped instead of dropped (br-r6awv).
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts)
                 VALUES
                    (1, 100, 10, 'Archive copy', 'archive body', 1771761600000000),
                    (2, 100, 10, 'DB-only', 'db body', 2)",
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
        let identity_rows = conn
            .query_sync(
                "SELECT p.id AS project_id, a.id AS agent_id
                 FROM agents a
                 JOIN projects p ON p.id = a.project_id
                 WHERE p.human_key = '/test-project' AND a.name = 'Bob'",
                &[],
            )
            .expect("query reconstructed recipient identity");
        assert_eq!(identity_rows.len(), 1);
        let reconstructed_project_id = identity_rows[0]
            .get_named::<i64>("project_id")
            .expect("project id");
        let reconstructed_bob_id = identity_rows[0]
            .get_named::<i64>("agent_id")
            .expect("agent id");
        assert_ne!(
            reconstructed_bob_id, 11,
            "the fixture must exercise numeric agent-id renumbering"
        );
        let runtime_conn = crate::DbConn::open_file(db_path.to_str().unwrap())
            .expect("open reconstructed mailbox through the runtime engine");
        let inbox = crate::sync::fetch_inbox_rows_from_conn(
            &runtime_conn,
            reconstructed_project_id,
            reconstructed_bob_id,
            false,
            false,
            false,
            None,
            25,
        )
        .expect("fetch inbox through the production query after reconstruction");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].message.subject, "Archive copy");
        assert!(
            crate::sync::fetch_inbox_rows_from_conn(
                &runtime_conn,
                reconstructed_project_id,
                11,
                false,
                false,
                false,
                None,
                25,
            )
            .expect("query stale source-local numeric id")
            .is_empty(),
            "delivery must follow the reconstructed stable identity, not the obsolete numeric id"
        );
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

    /// br-5mnkl: the salvage reads are keyset-paginated so the startup
    /// self-heal path can never materialize the whole `messages` /
    /// `message_recipients` tables at once. This exercises multiple batch
    /// boundaries for both tables and proves no row is dropped or
    /// double-merged at the seams.
    #[test]
    fn salvage_merge_pages_large_tables_across_keyset_batches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_paged.db");
        let salvage_db_path = tmp.path().join("salvage_paged.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&agent_dir).expect("create archive agent dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"test-project","human_key":"/test-project","created_at":0}"#,
        )
        .expect("write project.json");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-02-22T00:00:00Z","last_active_ts":"2026-02-22T00:00:00Z"}"#,
        )
        .expect("write profile.json");

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
                    (12, 100, 'Carol'),
                    (13, 100, 'Dave'),
                    (14, 100, 'Erin')",
                &[],
            )
            .unwrap();

        // 2 full message batches + a partial third; 4 recipients per message
        // crosses the (larger) recipient batch size at a non-aligned offset.
        let message_count = SALVAGE_MESSAGE_BATCH_ROWS * 2 + 7;
        let recipient_agent_ids = [11i64, 12, 13, 14];
        let recipient_count = message_count * recipient_agent_ids.len();
        assert!(
            recipient_count > SALVAGE_RECIPIENT_BATCH_ROWS,
            "fixture must force at least two recipient batches"
        );
        let message_values = (1..=message_count)
            .map(|id| format!("({id}, 100, 10, 'subject {id}', 'body {id}', {id})"))
            .collect::<Vec<_>>()
            .join(", ");
        salvage_conn
            .query_sync(
                &format!(
                    "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts) VALUES {message_values}"
                ),
                &[],
            )
            .unwrap();
        let recipient_values = (1..=message_count)
            .flat_map(|message_id| {
                recipient_agent_ids.iter().map(move |agent_id| {
                    format!("({message_id}, {agent_id}, 'to', {message_id}, NULL)")
                })
            })
            .collect::<Vec<_>>()
            .join(", ");
        salvage_conn
            .query_sync(
                &format!(
                    "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES {recipient_values}"
                ),
                &[],
            )
            .unwrap();

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("paged salvage merge should succeed");
        assert_eq!(stats.salvaged_messages, message_count);
        assert_eq!(stats.salvaged_recipients, recipient_count);

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let count_row = |sql: &str| -> i64 {
            conn.query_sync(sql, &[]).expect("count query")[0]
                .get_named::<i64>("n")
                .expect("count value")
        };
        assert_eq!(
            count_row("SELECT COUNT(*) AS n FROM messages"),
            i64::try_from(message_count).unwrap(),
            "every salvaged message must survive the batch seams exactly once"
        );
        assert_eq!(
            count_row("SELECT COUNT(*) AS n FROM message_recipients"),
            i64::try_from(recipient_count).unwrap(),
            "every salvaged recipient must survive the batch seams exactly once"
        );
        assert_eq!(
            count_row("SELECT COUNT(DISTINCT message_id) AS n FROM message_recipients"),
            i64::try_from(message_count).unwrap(),
        );
        // Boundary rows: the last message of a full batch and the first of the
        // next batch must both exist with intact content and recipient state.
        for boundary_id in [
            i64::try_from(SALVAGE_MESSAGE_BATCH_ROWS).unwrap(),
            i64::try_from(SALVAGE_MESSAGE_BATCH_ROWS + 1).unwrap(),
            i64::try_from(message_count).unwrap(),
        ] {
            let subject_rows = conn
                .query_sync(
                    "SELECT subject FROM messages WHERE id = ?",
                    &[Value::BigInt(boundary_id)],
                )
                .expect("boundary subject query");
            assert_eq!(subject_rows.len(), 1, "message {boundary_id} must exist");
            assert_eq!(
                subject_rows[0]
                    .get_named::<String>("subject")
                    .expect("subject"),
                format!("subject {boundary_id}")
            );
            let read_rows = conn
                .query_sync(
                    "SELECT COUNT(*) AS n FROM message_recipients WHERE message_id = ? AND read_ts = ?",
                    &[Value::BigInt(boundary_id), Value::BigInt(boundary_id)],
                )
                .expect("boundary recipient query");
            assert_eq!(
                read_rows[0].get_named::<i64>("n").expect("count"),
                i64::try_from(recipient_agent_ids.len()).unwrap(),
                "recipient state for message {boundary_id} must survive intact"
            );
        }
    }

    /// br-5mnkl: `collect_db_message_ids` is keyset-paginated; an exact
    /// batch-multiple table (including non-positive ids, which only the
    /// floorless first batch can see) must round-trip completely.
    #[test]
    fn collect_db_message_ids_pages_exact_batch_multiple_with_nonpositive_ids() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("ids_paged.db");
        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_raw("CREATE TABLE messages (id INTEGER PRIMARY KEY)")
            .unwrap();
        // -3, 0, and 1..=(BATCH-2): exactly one full batch, so the pager must
        // issue a second (empty) probe and terminate without dropping rows.
        let positive_max = i64::try_from(DB_MESSAGE_ID_BATCH_ROWS - 2).unwrap();
        let values = std::iter::once("(-3)".to_string())
            .chain(std::iter::once("(0)".to_string()))
            .chain((1..=positive_max).map(|id| format!("({id})")))
            .collect::<Vec<_>>()
            .join(", ");
        conn.query_sync(&format!("INSERT INTO messages (id) VALUES {values}"), &[])
            .unwrap();
        drop(conn);
        let admitted = crate::DbConn::open_file(db_path.to_string_lossy().as_ref())
            .expect("admit paged message-id fixture through FrankenSQLite");
        admitted
            .query_sync("SELECT COUNT(*) AS n FROM messages", &[])
            .expect("query admitted paged message-id fixture");
        crate::close_db_conn(admitted, "settle paged message-id fixture");

        let ids = collect_db_message_ids(&db_path).expect("collect ids");
        assert_eq!(ids.len(), DB_MESSAGE_ID_BATCH_ROWS);
        assert!(
            ids.contains(&-3),
            "floorless first batch must keep negative ids"
        );
        assert!(ids.contains(&0), "floorless first batch must keep id 0");
        assert!(ids.contains(&positive_max));
    }

    /// br-5mnkl: the warning vector itself must stay bounded on pathological
    /// archives; overflow is counted, not itemized, and surfaced in Display.
    #[test]
    fn push_warning_caps_itemization_and_counts_suppressed() {
        let mut stats = ReconstructStats::default();
        for i in 0..(MAX_ITEMIZED_WARNINGS + 7) {
            stats.push_warning(format!("warning {i}"));
        }
        assert_eq!(stats.warnings.len(), MAX_ITEMIZED_WARNINGS);
        assert_eq!(stats.suppressed_warnings, 7);
        assert_eq!(stats.warnings[0], "warning 0");
        assert_eq!(
            stats.warnings[MAX_ITEMIZED_WARNINGS - 1],
            format!("warning {}", MAX_ITEMIZED_WARNINGS - 1)
        );
        let display = stats.to_string();
        assert!(
            display.contains("suppressed 7 warning(s)"),
            "display must surface the suppressed count, got: {display}"
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
    fn reconstruct_with_salvage_remaps_same_project_identity_mismatch() {
        // br-r6awv: the candidate's slot for id 7 holds a *different* message
        // in the SAME project (canonical-id collision handling / row-id reuse
        // across generations). Salvage dedup must compare identity, not just
        // (id, project) — otherwise the DB-only message is silently dropped
        // as "already carried".
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_identity_mismatch.db");
        let salvage_db_path = tmp.path().join("salvage_identity_mismatch.db");
        let storage_root = tmp.path().join("storage");

        let archive_project = storage_root.join("projects").join("solo");
        let archive_messages = archive_project.join("messages").join("2026").join("07");
        std::fs::create_dir_all(&archive_messages).expect("create archive messages");
        std::fs::write(
            archive_project.join("project.json"),
            r#"{"slug":"solo","human_key":"/solo","created_at":1}"#,
        )
        .expect("write archive project");
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
                    (500, 'solo', '/solo', 1);
                 INSERT INTO agents
                    (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                    VALUES
                    (600, 500, 'Bob', 'coder', 'test', '', 1, 2, 'auto', 'auto');
                 INSERT INTO messages
                    (id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments)
                    VALUES
                    (7, 500, 600, 'DB-only different message', 'db body', 'normal', 0, 99,
                     '{\"to\":[],\"cc\":[],\"bcc\":[]}', '[]');",
            )
            .expect("seed same-project identity-mismatch message");

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("same-project identity mismatch must remap, not drop");
        assert_eq!(
            stats.salvaged_messages, 1,
            "DB-only message must survive; warnings: {:?}",
            stats.warnings
        );
        assert_eq!(stats.salvaged_message_id_remaps, 1);

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT id, subject FROM messages ORDER BY id", &[])
            .expect("query messages");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get_named::<i64>("id").unwrap(), 7);
        assert_eq!(
            rows[0].get_named::<String>("subject").unwrap(),
            "Archive message"
        );
        assert!(rows[1].get_named::<i64>("id").unwrap() > 7);
        assert_eq!(
            rows[1].get_named::<String>("subject").unwrap(),
            "DB-only different message"
        );
    }

    #[test]
    fn reconstruct_with_salvage_maps_same_identity_under_a_different_numeric_id() {
        // Archive carried the message as canonical id 7; the salvage source
        // still has the same identity under id 99 (row-id reuse). Re-inserting
        // it trips the promotion duplicate-inflation guard (br-r6awv).
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_identity_remap.db");
        let salvage_db_path = tmp.path().join("salvage_identity_remap.db");
        let storage_root = tmp.path().join("storage");

        let archive_project = storage_root.join("projects").join("solo");
        let archive_messages = archive_project.join("messages").join("2026").join("07");
        std::fs::create_dir_all(&archive_messages).expect("create archive messages");
        std::fs::write(
            archive_project.join("project.json"),
            r#"{"slug":"solo","human_key":"/solo","created_at":1}"#,
        )
        .expect("write archive project");
        std::fs::write(
            archive_messages.join("2026-07-17T12-00-00Z__archive__7.md"),
            r#"---json
{"id":7,"from":"Alice","to":[],"subject":"Archive message","importance":"normal","created_ts":1784289600000000,"attachments":[]}
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
                    (500, 'solo', '/solo', 1);
                 INSERT INTO agents
                    (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                    VALUES
                    (600, 500, 'Alice', 'coder', 'test', '', 1, 2, 'auto', 'auto');
                 INSERT INTO messages
                    (id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments)
                    VALUES
                    (99, 500, 600, 'Archive message', 'archive body', 'normal', 0, 1784289600000000,
                     '{\"to\":[],\"cc\":[],\"bcc\":[]}', '[]'),
                    (100, 500, 600, 'DB-only', 'db body', 'normal', 0, 2,
                     '{\"to\":[],\"cc\":[],\"bcc\":[]}', '[]');",
            )
            .expect("seed same-identity different-id salvage rows");

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("same-identity salvage row must map, not re-insert");
        assert_eq!(
            stats.salvaged_messages, 1,
            "only the true DB-only row is new; warnings: {:?}",
            stats.warnings
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT id, subject FROM messages ORDER BY id", &[])
            .expect("query messages");
        assert_eq!(
            rows.len(),
            2,
            "must not replay the archive copy as a second row"
        );
        assert_eq!(rows[0].get_named::<i64>("id").unwrap(), 7);
        assert_eq!(
            rows[0].get_named::<String>("subject").unwrap(),
            "Archive message"
        );
        assert_eq!(rows[1].get_named::<String>("subject").unwrap(), "DB-only");
    }

    #[test]
    fn reconstruct_with_salvage_product_created_at_is_deterministic_when_column_is_absent() {
        // products.created_at is part of the unique promotion fingerprint.
        // A now_micros() fallback would mint a new identity on every recovery
        // and refuse promotion as a phantom loss (br-r6awv).
        let tmp = tempfile::tempdir().expect("tempdir");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects")).expect("archive root");
        let salvage_db_path = tmp.path().join("salvage_product_no_created_at.db");

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE products (
                    id INTEGER PRIMARY KEY,
                    product_uid TEXT NOT NULL,
                    name TEXT NOT NULL
                );",
            )
            .expect("products table without created_at");
        salvage_conn
            .execute_raw(
                "INSERT INTO products (id, product_uid, name) VALUES (1, 'uid-widget', 'Widget');",
            )
            .expect("seed product");
        drop(salvage_conn);

        let mut created_ats = Vec::new();
        for name in ["first.db", "second.db"] {
            let db_path = tmp.path().join(name);
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("missing product created_at must not block salvage");
            let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
            let rows = conn
                .query_sync(
                    "SELECT product_uid, name, CAST(created_at AS INTEGER) AS created_at \
                     FROM products",
                    &[],
                )
                .expect("query salvaged product");
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].get_named::<String>("product_uid").unwrap(),
                "uid-widget"
            );
            assert_eq!(rows[0].get_named::<String>("name").unwrap(), "Widget");
            created_ats.push(rows[0].get_named::<i64>("created_at").unwrap());
        }
        assert_eq!(created_ats[0], 0, "absent created_at must fall back to 0");
        assert_eq!(
            created_ats[0], created_ats[1],
            "two recoveries must not mint distinct product identities"
        );
    }

    #[test]
    fn reconstruct_with_salvage_reservation_created_ts_is_deterministic_when_column_is_absent() {
        // Reservation identity includes created_ts. A required-column hard
        // error here is not corruption-class, so reconstruct would refuse
        // the archive-only candidate and wedge doctor. Two recoveries of a
        // legacy schema without created_ts must land 0 both times.
        let tmp = tempfile::tempdir().expect("tempdir");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects")).expect("archive root");
        let salvage_db_path = tmp.path().join("salvage_reservation_no_created_ts.db");

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    human_key TEXT,
                    created_at INTEGER
                );
                CREATE TABLE agents (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    name TEXT NOT NULL
                );
                CREATE TABLE file_reservations (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    agent_id INTEGER NOT NULL,
                    path_pattern TEXT NOT NULL,
                    exclusive INTEGER NOT NULL,
                    reason TEXT NOT NULL
                );",
            )
            .expect("legacy reservations table without created_ts");
        salvage_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (1, 'legacy-res', '/legacy-res', 1);
                 INSERT INTO agents (id, project_id, name) VALUES (2, 1, 'CoralMarsh');
                 INSERT INTO file_reservations
                    (id, project_id, agent_id, path_pattern, exclusive, reason)
                 VALUES (9, 1, 2, 'src/**', 1, 'legacy');",
            )
            .expect("seed legacy reservation");
        drop(salvage_conn);

        let mut created_ats = Vec::new();
        for name in ["first.db", "second.db"] {
            let db_path = tmp.path().join(name);
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("missing reservation created_ts must not block salvage");
            let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
            let rows = conn
                .query_sync(
                    "SELECT path_pattern, CAST(created_ts AS INTEGER) AS created_ts, \
                            CAST(expires_ts AS INTEGER) AS expires_ts \
                     FROM file_reservations",
                    &[],
                )
                .expect("query salvaged reservation");
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].get_named::<String>("path_pattern").unwrap(),
                "src/**"
            );
            created_ats.push(rows[0].get_named::<i64>("created_ts").unwrap());
            assert_eq!(
                rows[0].get_named::<i64>("expires_ts").unwrap(),
                created_ats[created_ats.len() - 1],
                "missing expires_ts must follow the deterministic created_ts"
            );
        }
        assert_eq!(
            created_ats[0], 0,
            "absent reservation created_ts must fall back to 0"
        );
        assert_eq!(
            created_ats[0], created_ats[1],
            "two recoveries must not mint distinct reservation identities"
        );
    }

    #[test]
    fn reconstruct_with_salvage_reservation_missing_exclusive_defaults_deterministically() {
        // Exclusive is part of reservation identity. A required-column hard
        // error here is not corruption-class, so reconstruct would refuse
        // the archive-only candidate and wedge doctor. Two recoveries of a
        // legacy schema without exclusive must land the archive default (1).
        let tmp = tempfile::tempdir().expect("tempdir");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects")).expect("archive root");
        let salvage_db_path = tmp.path().join("salvage_reservation_no_exclusive.db");

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    human_key TEXT,
                    created_at INTEGER
                );
                CREATE TABLE agents (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    name TEXT NOT NULL
                );
                CREATE TABLE file_reservations (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    agent_id INTEGER NOT NULL,
                    path_pattern TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    created_ts INTEGER NOT NULL
                );",
            )
            .expect("legacy reservations table without exclusive");
        salvage_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (1, 'legacy-excl', '/legacy-excl', 1);
                 INSERT INTO agents (id, project_id, name) VALUES (2, 1, 'CoralMarsh');
                 INSERT INTO file_reservations
                    (id, project_id, agent_id, path_pattern, reason, created_ts)
                 VALUES (9, 1, 2, 'src/**', 'legacy', 10);",
            )
            .expect("seed legacy reservation");
        drop(salvage_conn);

        let mut exclusives = Vec::new();
        for name in ["first.db", "second.db"] {
            let db_path = tmp.path().join(name);
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("missing reservation exclusive must not block salvage");
            let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
            let rows = conn
                .query_sync(
                    "SELECT path_pattern, CAST(exclusive AS INTEGER) AS exclusive, \
                            CAST(created_ts AS INTEGER) AS created_ts \
                     FROM file_reservations",
                    &[],
                )
                .expect("query salvaged reservation");
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].get_named::<String>("path_pattern").unwrap(),
                "src/**"
            );
            assert_eq!(rows[0].get_named::<i64>("created_ts").unwrap(), 10);
            exclusives.push(rows[0].get_named::<i64>("exclusive").unwrap());
        }
        assert_eq!(
            exclusives[0], 1,
            "absent exclusive must fall back to the archive default (true/1)"
        );
        assert_eq!(
            exclusives[0], exclusives[1],
            "two recoveries must not mint distinct exclusive identities"
        );
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
                "SELECT p.slug, a.id AS agent_id, a.name, fr.path_pattern, fr.exclusive, fr.reason,
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
        assert_ne!(
            rows[0].get_named::<i64>("agent_id").unwrap(),
            200,
            "the fixture must prove reservation ownership survives numeric agent-id renumbering"
        );
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
    fn reconstruct_with_salvage_keeps_exclusive_and_shared_reservation_identities() {
        // Exclusive is part of reservation identity. Two salvage rows that
        // share path+created_ts but differ on exclusive are two leases, not
        // an ambiguous key that should refuse the whole reconstruct.
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
                    (900, 100, 200, 'src/**', 1, 'exclusive lease', 10, 1000, NULL),
                    (901, 100, 200, 'src/**', 0, 'shared lease', 10, 1000, NULL);",
            )
            .expect("seed exclusive and shared reservation identities");

        reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
            .expect("exclusive vs shared on the same path must not refuse reconstruct");

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT exclusive, reason FROM file_reservations ORDER BY exclusive DESC",
                &[],
            )
            .expect("query salvaged reservations");
        assert_eq!(rows.len(), 2, "both exclusive modes must be recovered");
        assert_eq!(rows[0].get_named::<i64>("exclusive").unwrap(), 1);
        assert_eq!(
            rows[0].get_named::<String>("reason").unwrap(),
            "exclusive lease"
        );
        assert_eq!(rows[1].get_named::<i64>("exclusive").unwrap(), 0);
        assert_eq!(
            rows[1].get_named::<String>("reason").unwrap(),
            "shared lease"
        );
    }

    #[test]
    fn reconstruct_with_salvage_keeps_going_when_release_ledger_timestamps_conflict() {
        // released_ts is volatile (GH#208). A ledger/row mismatch used to
        // fail the whole salvage merge, which then refused the archive-only
        // candidate and wedged doctor (br-r6awv).
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_release_conflict.db");
        let salvage_db_path = tmp.path().join("salvage_release_conflict.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("release-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let reservations_dir = project_dir.join("file_reservations");
        std::fs::create_dir_all(&agent_dir).expect("create archive agent");
        std::fs::create_dir_all(&reservations_dir).expect("create reservations dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"release-project","human_key":"/release-project","created_at":1}"#,
        )
        .expect("write archive project");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":1,"last_active_ts":2}"#,
        )
        .expect("write archive agent");
        std::fs::write(
            reservations_dir.join("id-900.json"),
            r#"{
                "id": 900,
                "project": "/release-project",
                "agent": "Alice",
                "path_pattern": "src/**",
                "exclusive": true,
                "reason": "archive lease",
                "created_ts": 10,
                "expires_ts": 1000,
                "released_ts": 100
            }"#,
        )
        .expect("write archive reservation with released_ts");

        let salvage_conn = SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(&crate::schema::init_schema_sql_base())
            .expect("init salvage schema");
        salvage_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at)
                    VALUES (100, 'release-project', '/release-project', 1);
                 INSERT INTO agents
                    (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                    VALUES (200, 100, 'Alice', 'coder', 'test', '', 1, 2, 'auto', 'auto');
                 INSERT INTO file_reservations
                    (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts)
                    VALUES (900, 100, 200, 'src/**', 1, 'archive lease', 10, 1000, 100);
                 INSERT INTO file_reservation_releases (reservation_id, released_ts)
                    VALUES (900, 200);",
            )
            .expect("seed conflicting release ledger");

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("volatile release-ledger mismatch must not refuse reconstruct");
        assert!(
            stats.warnings.iter().any(|warning| {
                warning.contains("conflicting row/ledger release timestamp")
                    || warning.contains("conflicting terminal release ledger timestamp")
            }),
            "mismatch should be recorded as a warning, not a hard error: {:?}",
            stats.warnings
        );

        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT CAST(fr.released_ts AS INTEGER) AS row_released_ts, \
                        rr.released_ts AS ledger_released_ts \
                 FROM file_reservations fr \
                 LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id",
                &[],
            )
            .expect("query recovered reservation");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<i64>("row_released_ts").unwrap(),
            100,
            "archive candidate released_ts must be kept"
        );
        assert!(
            rows[0].get_named::<i64>("ledger_released_ts").is_err(),
            "conflicting salvage ledger must not overwrite the archive candidate"
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

        FAIL_SALVAGE_MERGE_AFTER_PROJECTS.with(|hook| hook.set(true));
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
    fn reconstruct_with_salvage_degrades_to_archive_when_message_query_is_corrupt() {
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

        FAIL_SALVAGE_QUERY_MESSAGES.with(|hook| hook.set(true));
        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("corrupt salvage merge must not block an archive-only heal");

        assert!(
            db_path.exists(),
            "archive-only candidate must exist so doctor can promote a heal"
        );
        assert!(
            stats.warnings.iter().any(|warning| {
                warning.contains("failed with corruption")
                    && warning.contains("database disk image is malformed")
            }),
            "stats must attest the salvage-merge degrade, got {:?}",
            stats.warnings
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
    fn reconstruct_with_salvage_degrades_to_archive_when_source_is_not_sqlite() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_unreadable_salvage.db");
        let salvage_db_path = tmp.path().join("not-a-database.db");
        let storage_root = tmp.path().join("archive");
        std::fs::create_dir(&storage_root).expect("archive root");
        std::fs::write(&salvage_db_path, b"not-a-database").expect("plant unreadable salvage");

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("unreadable salvage must not block archive rebuild");
        assert!(
            db_path.exists(),
            "archive-only candidate must exist so doctor can promote a heal"
        );
        assert!(
            stats.warnings.iter().any(|warning| {
                warning.contains("salvage source skipped")
                    && (warning.contains("not a database") || warning.contains("unreadable"))
            }),
            "stats must attest the salvage degrade, got {:?}",
            stats.warnings
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn reconstruct_with_salvage_never_validates_a_lossy_utf8_alias() {
        use std::os::unix::ffi::OsStringExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_non_utf8_salvage.db");
        let storage_root = tmp.path().join("archive");
        std::fs::create_dir(&storage_root).expect("archive root");

        let invalid_name = std::ffi::OsString::from_vec(b"salvage-\xFF.sqlite3".to_vec());
        let salvage_db_path = tmp.path().join(&invalid_name);
        std::fs::write(&salvage_db_path, b"bytes that were never validated")
            .expect("write invalid-byte salvage source");

        let lossy_alias = tmp.path().join(invalid_name.to_string_lossy().into_owned());
        let alias_conn = SqliteDbConn::open_file(
            lossy_alias
                .to_str()
                .expect("lossy alias should be valid UTF-8"),
        )
        .expect("open healthy lossy alias");
        alias_conn
            .execute_raw("CREATE TABLE marker(value TEXT NOT NULL);")
            .expect("create marker table in alias");
        drop(alias_conn);

        let error =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect_err("salvage must fail closed instead of inspecting the U+FFFD alias");
        assert!(
            error.to_string().contains("not valid UTF-8"),
            "unexpected salvage validation error: {error}"
        );
        assert!(
            !db_path.exists(),
            "a path-identity failure must not create a promotable candidate"
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn reconstruct_target_never_mutates_a_lossy_utf8_alias() {
        use std::os::unix::ffi::OsStringExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let storage_root = tmp.path().join("archive");
        write_archive_message(&storage_root, "test-project", 1);

        let invalid_name = std::ffi::OsString::from_vec(b"target-\xFF.sqlite3".to_vec());
        let db_path = tmp.path().join(&invalid_name);
        let lossy_alias = tmp.path().join(invalid_name.to_string_lossy().into_owned());
        let alias_conn = SqliteDbConn::open_file(
            lossy_alias
                .to_str()
                .expect("lossy alias should be valid UTF-8"),
        )
        .expect("open healthy lossy alias");
        alias_conn
            .execute_raw(
                "CREATE TABLE marker(value TEXT NOT NULL); INSERT INTO marker VALUES ('untouched')",
            )
            .expect("seed alias marker");
        drop(alias_conn);

        let error = reconstruct_from_archive(&db_path, &storage_root)
            .expect_err("reconstruct must reject a non-UTF-8 target identity");
        assert!(
            error.to_string().contains("not valid UTF-8"),
            "unexpected target validation error: {error}"
        );
        assert!(
            !db_path.exists(),
            "a rejected target must not even be claimed as an empty file"
        );
        let alias_conn = SqliteDbConn::open_file(lossy_alias.to_str().unwrap()).unwrap();
        let rows = alias_conn
            .query_sync("SELECT value FROM marker", &[])
            .expect("read alias marker");
        assert_eq!(
            rows[0].get_named::<String>("value").unwrap(),
            "untouched",
            "reconstruct must not redirect writes into the U+FFFD sibling"
        );
    }

    #[test]
    fn reconstruct_with_salvage_degrades_to_archive_when_source_fails_integrity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_integrity_failed_salvage.db");
        let salvage_db_path = tmp.path().join("salvage_integrity_failed.db");
        let storage_root = tmp.path().join("archive");
        std::fs::create_dir(&storage_root).expect("archive root");

        let marker = "AM_R6AWV_SALVAGE_INTEGRITY_MARKER_0123456789AB";
        let replacement = "AM_R6AWV_SALVAGE_INTEGRITY_MARKER_BA9876543210";
        assert_eq!(marker.len(), replacement.len());

        let salvage_conn =
            SqliteDbConn::open_file(salvage_db_path.to_str().unwrap()).expect("open salvage");
        salvage_conn
            .execute_raw("PRAGMA journal_mode = DELETE;")
            .expect("stable image for byte flip");
        salvage_conn
            .execute_raw(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY,
                    subject TEXT
                )",
            )
            .expect("create messages");
        salvage_conn
            .execute_raw(&format!(
                "INSERT INTO messages (id, subject) VALUES (1, '{marker}')"
            ))
            .expect("plant indexed subject");
        salvage_conn
            .execute_raw("CREATE INDEX idx_messages_subject ON messages(subject)")
            .expect("index to diverge");
        drop(salvage_conn);

        let mut bytes = std::fs::read(&salvage_db_path).expect("read salvage bytes");
        let offsets = bytes
            .windows(marker.len())
            .enumerate()
            .filter_map(|(offset, window)| (window == marker.as_bytes()).then_some(offset))
            .collect::<Vec<_>>();
        assert!(
            offsets.len() >= 2,
            "marker must appear in table and index, found {}",
            offsets.len()
        );
        let target = offsets[offsets.len() - 1];
        bytes[target..target + marker.len()].copy_from_slice(replacement.as_bytes());
        std::fs::write(&salvage_db_path, bytes).expect("write integrity-failed salvage");

        assert!(
            !crate::pool::sqlite_file_passes_full_integrity_check(&salvage_db_path)
                .expect("full integrity probe"),
            "planted index/table divergence must fail full integrity_check"
        );

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("integrity-failed salvage must not block archive rebuild");
        assert!(
            db_path.exists(),
            "archive-only candidate must exist so doctor can promote a heal"
        );
        assert!(
            stats.warnings.iter().any(|warning| {
                warning.contains("salvage source skipped")
                    && warning.contains("failed full integrity_check")
            }),
            "stats must attest the integrity degrade, got {:?}",
            stats.warnings
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
    fn reconstruct_with_salvage_preserves_agent_lifecycle_restrictions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_lifecycle.db");
        let salvage_db_path = tmp.path().join("salvage_lifecycle.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("lifecycle-project");
        let alice_dir = project_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&alice_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"lifecycle-project","human_key":"/lifecycle-project","created_at":1}"#,
        )
        .unwrap();
        // The archive candidate is active. A readable salvage database that
        // records a restriction must not be allowed to reactivate it.
        std::fs::write(
            alice_dir.join("profile.json"),
            r#"{
                "name":"Alice",
                "program":"codex-cli",
                "model":"gpt-5",
                "inception_ts":1,
                "last_active_ts":1,
                "attachments_policy":"auto",
                "contact_policy":"auto"
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
        // TEXT lifecycle timestamps model the Python/legacy import shape; the
        // target schema stores canonical integer microseconds.
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
                    contact_policy TEXT,
                    retired_at TEXT
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agent_deregistrations (
                    agent_id INTEGER PRIMARY KEY,
                    deregistered_at TEXT
                )",
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (100, 'lifecycle-project', '/lifecycle-project', 1)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agents
                 (id, project_id, name, program, model, task_description,
                  inception_ts, last_active_ts, attachments_policy, contact_policy, retired_at)
                 VALUES
                    (10, 100, 'Alice', 'codex-cli', 'gpt-5', '',
                     1, 2000000, 'auto', 'auto', '1970-01-01T00:00:02Z'),
                    (11, 100, 'Bob', 'codex-cli', 'gpt-5', '',
                     1, 3000000, 'auto', 'block_all', NULL),
                    (12, 100, 'Carol', 'codex-cli', 'gpt-5',
                     '[DEREGISTERED 1970-01-01T00:00:04Z] legacy',
                     1, 4000000, 'auto', 'block_all', NULL),
                    (13, 100, 'Dave', 'codex-cli', 'gpt-5', '',
                     1, 5000000, 'auto', 'auto', 'not-a-timestamp')",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agent_deregistrations (agent_id, deregistered_at)
                 VALUES (11, '1970-01-01 00:00:03Z')",
                &[],
            )
            .unwrap();
        drop(salvage_conn);

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("salvage merge should preserve lifecycle state");
        assert!(
            stats
                .warnings
                .iter()
                .any(|warning| warning.contains("unparseable retired_at")),
            "malformed retirement data must be preserved fail-closed and reported"
        );

        // Reopen from disk so the assertion also covers committed restart
        // state rather than only the connection that performed the merge.
        let conn = SqliteDbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT name, retired_at FROM agents ORDER BY name", &[])
            .unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].get_named::<String>("name").unwrap(), "Alice");
        assert_eq!(
            rows[0].get_named::<Option<i64>>("retired_at").unwrap(),
            Some(2_000_000)
        );
        assert_eq!(
            rows[1].get_named::<Option<i64>>("retired_at").unwrap(),
            None
        );
        assert_eq!(
            rows[2].get_named::<Option<i64>>("retired_at").unwrap(),
            None
        );
        assert_eq!(rows[3].get_named::<String>("name").unwrap(), "Dave");
        assert_eq!(
            rows[3].get_named::<Option<i64>>("retired_at").unwrap(),
            Some(5_000_000)
        );

        let deregistrations = conn
            .query_sync(
                "SELECT a.name AS name, d.deregistered_at AS deregistered_at
                 FROM agent_deregistrations d
                 JOIN agents a ON a.id = d.agent_id
                 ORDER BY a.name",
                &[],
            )
            .unwrap();
        assert_eq!(deregistrations.len(), 2);
        assert_eq!(
            deregistrations[0].get_named::<String>("name").unwrap(),
            "Bob"
        );
        assert_eq!(
            deregistrations[0]
                .get_named::<i64>("deregistered_at")
                .unwrap(),
            3_000_000
        );
        assert_eq!(
            deregistrations[1].get_named::<String>("name").unwrap(),
            "Carol"
        );
        assert_eq!(
            deregistrations[1]
                .get_named::<i64>("deregistered_at")
                .unwrap(),
            4_000_000
        );
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
        drop(conn);
        let admitted = crate::DbConn::open_file(db_path.to_string_lossy().as_ref())
            .expect("admit message inventory fixture through FrankenSQLite");
        admitted
            .query_sync("SELECT COUNT(*) AS n FROM messages", &[])
            .expect("query admitted message inventory fixture");
        crate::close_db_conn(admitted, "settle message inventory fixture");
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
        let admitted = crate::DbConn::open_file(db_path.to_string_lossy().as_ref())
            .expect("admit missing-table fixture through FrankenSQLite");
        admitted
            .query_sync("SELECT COUNT(*) AS n FROM dummy", &[])
            .expect("query admitted missing-table fixture");
        crate::close_db_conn(admitted, "settle missing-table fixture");
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

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn collect_db_message_ids_never_reads_a_lossy_utf8_alias() {
        use std::os::unix::ffi::OsStringExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let invalid_name = std::ffi::OsString::from_vec(b"mailbox-\xFF.sqlite3".to_vec());
        let db_path = tmp.path().join(&invalid_name);
        std::fs::write(&db_path, b"raw non-UTF path witness").unwrap();
        let lossy_alias = tmp.path().join(invalid_name.to_string_lossy().into_owned());
        setup_db_with_messages(&lossy_alias, &[5, 15, 25]);

        let error = collect_db_message_ids(&db_path)
            .expect_err("inventory must reject rather than inspect the U+FFFD sibling");
        assert!(
            error.to_string().contains("not valid UTF-8"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read(&db_path).unwrap(),
            b"raw non-UTF path witness",
            "the exact raw path must remain untouched"
        );
        assert_eq!(
            collect_db_message_ids(&lossy_alias).unwrap(),
            BTreeSet::from([5, 15, 25]),
            "the healthy alias remains a distinct database"
        );
    }

    #[test]
    fn reconstruct_rejects_stale_certification_and_namespace_target_state() {
        let tmp = tempfile::tempdir().unwrap();
        let storage_root = tmp.path().join("archive");

        for suffix in ["-wal-cert", "-fsqlite-ns-use"] {
            let db_path = tmp.path().join(format!(
                "candidate-{}.sqlite3",
                suffix.trim_start_matches('-')
            ));
            let companion = crate::pool::sqlite_path_with_suffix(&db_path, suffix);
            std::fs::write(&companion, b"stale generation witness").unwrap();

            let error = reconstruct_from_archive(&db_path, &storage_root)
                .expect_err("stale companion state must block target admission");
            assert!(
                error.to_string().contains("companion state"),
                "unexpected error for {suffix}: {error}"
            );
            assert!(
                !db_path.exists(),
                "reconstruction must not claim a main file beside stale {suffix} state"
            );
            assert_eq!(
                std::fs::read(&companion).unwrap(),
                b"stale generation witness"
            );
        }
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
        assert_eq!(
            json["db_only_ids"].as_array().unwrap().as_slice(),
            [] as [serde_json::Value; 0]
        );
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
