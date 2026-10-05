//! Reconstruct missing stable ACTIVE reservation artifacts without client traffic.
//!
//! Publication is create-only: ordinary reservation writers do not all take the
//! project lock, so an active repair must never replace an existing file. A
//! concurrent release/renewal wins the destination instead. Only generation-
//! stamped stable paths are repaired; reusable digest aliases are never written.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use asupersync::{Cx, Outcome};
use fastmcp_core::block_on;
use git2::{ObjectType, Oid, Repository};
use mcp_agent_mail_core::{Config, reservation_artifact::reservation_artifact_filename};
use mcp_agent_mail_db::{DbError, DbPool, corruption_circuit_breaker};
use serde::Serialize;
use serde_json::{Value, json};

use super::agent_reconcile::{committed_artifact, head_tree, read_artifact};

const IDS_PER_PASS: i64 = 32;
const WRITES_PER_PASS: usize = 4;
const SOURCE_BYTES: i64 = 32 * 1024;
const ARTIFACT_BYTES: usize = 128 * 1024;
const GRANT_GRACE_US: i64 = 30 * 1_000_000;

/// Finite ID rounds keep older missing artifacts reachable under new grants.
#[derive(Debug, Default)]
pub struct ActiveReservationCursor {
    identity: String,
    after: i64,
    ceiling: Option<i64>,
}

/// A bounded pass, not a certificate that every active lease is archived.
#[derive(Debug, Default, Serialize)]
pub struct ActiveReservationReport {
    pub scanned: usize,
    pub unchanged: usize,
    pub attempted: usize,
    pub repaired: usize,
    pub deferred: usize,
    pub more: bool,
    pub interrupted: bool,
}

#[derive(Debug, PartialEq)]
struct Source {
    id: i64,
    project_id: i64,
    agent_id: i64,
    slug: String,
    generation: String,
    artifact: Value,
}

fn invalid(message: impl Into<String>) -> crate::StorageError {
    crate::StorageError::InvalidPath(message.into())
}

fn source_error(error: impl std::fmt::Display) -> String {
    let error = DbError::Sqlite(error.to_string());
    corruption_circuit_breaker().observe_error(&error);
    format!("active reservation source unavailable: {error}")
}

fn outcome<T, E: std::fmt::Display>(value: Outcome<T, E>) -> Result<T, String> {
    match value {
        Outcome::Ok(value) => Ok(value),
        Outcome::Err(error) => Err(source_error(error)),
        Outcome::Cancelled(_) => Err("active reservation repair cancelled".into()),
        Outcome::Panicked(_) => Err("active reservation source panicked".into()),
    }
}

fn timestamp(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        value.as_str().and_then(|text| {
            text.trim()
                .parse()
                .ok()
                .or_else(|| mcp_agent_mail_core::iso_to_micros(text.trim()))
        })
    })
}

fn validate_source(cx: &Cx, pool: &DbPool, config: &Config) -> Result<(), String> {
    let selected =
        mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url)
            .ok_or("active reservation repair requires a file-backed mailbox")?;
    if fs::canonicalize(selected).map_err(source_error)?
        != fs::canonicalize(pool.sqlite_path()).map_err(source_error)?
        || pool.search_identity_path() != pool.sqlite_path()
        || fs::canonicalize(pool.storage_root()).map_err(source_error)?
            != fs::canonicalize(&config.storage_root).map_err(source_error)?
    {
        return Err("active reservation repair refuses a foreign source or archive root".into());
    }
    let conn = outcome(block_on(pool.acquire(cx)))?;
    let rows = conn
        .query_sync("PRAGMA query_only", &[])
        .map_err(source_error)?;
    if rows.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
        return Err("query-only snapshots cannot authorize active reservation repair".into());
    }
    Ok(())
}

fn read_source(cx: &Cx, pool: &DbPool, id: i64) -> Result<Option<Source>, String> {
    let conn = outcome(block_on(pool.acquire(cx)))?;
    // One SQL snapshot supplies the holder, generation and both release stores.
    // The size predicate bounds materialized strings, not the SQL engine's work.
    let rows = conn.query_sync(
        "SELECT fr.id, fr.project_id, fr.agent_id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                fr.created_ts, fr.expires_ts, CAST(fr.released_ts AS TEXT) AS hot_release, \
                rr.released_ts AS ledger_release, p.slug, p.human_key, a.name, d.generation_id \
         FROM file_reservations fr JOIN projects p ON p.id = fr.project_id \
         JOIN agents a ON a.id = fr.agent_id AND a.project_id = fr.project_id \
         LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
         LEFT JOIN db_identity d ON d.singleton = 0 \
         WHERE fr.id = ? AND length(CAST(fr.path_pattern AS BLOB)) + length(CAST(fr.reason AS BLOB)) + \
               length(CAST(p.slug AS BLOB)) + length(CAST(p.human_key AS BLOB)) + length(CAST(a.name AS BLOB)) + \
               length(CAST(fr.created_ts AS BLOB)) + length(CAST(fr.expires_ts AS BLOB)) + \
               length(CAST(fr.\"exclusive\" AS BLOB)) + \
               COALESCE(length(CAST(fr.released_ts AS BLOB)), 0) + \
               COALESCE(length(CAST(rr.released_ts AS BLOB)), 0) + \
               COALESCE(length(CAST(d.generation_id AS BLOB)), 0) <= ?",
        &[id.into(), SOURCE_BYTES.into()],
    ).map_err(source_error)?;
    let [row] = rows.as_slice() else {
        return Err(
            "active reservation source missing, oversized, or without holder authority".into(),
        );
    };
    let integer = |key| row.get_named::<i64>(key).map_err(source_error);
    let text = |key| row.get_named::<String>(key).map_err(source_error);
    let hot = row
        .get_named::<Option<String>>("hot_release")
        .map_err(source_error)?;
    let ledger = row
        .get_named::<Option<i64>>("ledger_release")
        .map_err(source_error)?;
    // A malformed release is uncertainty, never proof that this lease is active.
    if let Some(hot) = hot {
        let hot =
            timestamp(&Value::String(hot)).ok_or("malformed reservation release timestamp")?;
        if hot > 0 {
            return Ok(None);
        }
    }
    if let Some(ledger) = ledger {
        if ledger > 0 {
            return Ok(None);
        }
        return Err("malformed reservation release ledger timestamp".into());
    }
    let created = integer("created_ts")?;
    let expires = integer("expires_ts")?;
    let now = mcp_agent_mail_db::now_micros();
    if expires <= now || created > now.saturating_sub(GRANT_GRACE_US) {
        return Ok(None);
    }
    let generation = row
        .get_named::<Option<String>>("generation_id")
        .map_err(source_error)?
        .ok_or("active reservation repair requires a database generation")?;
    if generation.is_empty()
        || generation.len() > 128
        || !generation.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("active reservation database generation is invalid".into());
    }
    let slug = text("slug")?;
    let name = text("name")?;
    crate::validate_archive_component("project slug", &slug).map_err(source_error)?;
    crate::validate_archive_component("agent name", &name).map_err(source_error)?;
    let key = text("human_key")?;
    let pattern = text("path_pattern")?;
    let compiled = mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(pattern.trim());
    let exclusive = integer("exclusive")?;
    if id <= 0
        || integer("id")? != id
        || integer("project_id")? <= 0
        || integer("agent_id")? <= 0
        || !Path::new(&key).is_absolute()
        || pattern.trim().is_empty()
        || pattern.contains("..")
        || Path::new(pattern.trim()).is_absolute()
        || !compiled.is_matchable()
        || !matches!(exclusive, 0 | 1)
        || created <= 0
        || expires <= created
        || [created, expires]
            .into_iter()
            .any(|ts| chrono::DateTime::from_timestamp_micros(ts).is_none())
    {
        return Err("active reservation identity or timestamps are invalid".into());
    }
    Ok(Some(Source {
        id,
        project_id: integer("project_id")?,
        agent_id: integer("agent_id")?,
        slug,
        generation: generation.clone(),
        artifact: json!({
            "id": id, "project": key, "agent": name, "path_pattern": pattern.trim(),
            "exclusive": exclusive != 0, "reason": text("reason")?,
            "created_ts": mcp_agent_mail_db::micros_to_iso(created),
            "expires_ts": mcp_agent_mail_db::micros_to_iso(expires),
            "db_generation": generation,
        }),
    }))
}

fn validate_evidence(source: &Source, value: &Value) -> crate::Result<()> {
    for field in [
        "id",
        "project",
        "agent",
        "path_pattern",
        "exclusive",
        "db_generation",
    ] {
        if value.get(field) != source.artifact.get(field) {
            return Err(invalid(format!(
                "active reservation {field} conflicts; evidence preserved"
            )));
        }
    }
    if timestamp(&value["created_ts"]) != timestamp(&source.artifact["created_ts"]) {
        return Err(invalid(
            "active reservation creation identity conflicts; preserved",
        ));
    }
    if !value["released_ts"].is_null() && timestamp(&value["released_ts"]).is_none_or(|ts| ts > 0) {
        return Err(invalid(
            "terminal or malformed archive release cannot become active",
        ));
    }
    if timestamp(&value["expires_ts"])
        .is_none_or(|ts| Some(ts) > timestamp(&source.artifact["expires_ts"]))
    {
        return Err(invalid(
            "archive expiry is missing or newer than the live source; preserved",
        ));
    }
    Ok(())
}

/// Publish complete bytes only to an absent destination. A competing writer, including
/// one that publishes a release after our last DB read, always keeps its file.
fn publish_missing(path: &Path, bytes: &[u8]) -> crate::Result<()> {
    crate::ensure_parent_dir(path)?;
    if crate::path_existing_prefix_has_symlink(path)? {
        return Err(invalid("active reservation destination contains a symlink"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("reservation has no parent directory"))?;
    let mut staged = tempfile::Builder::new()
        .prefix(".active-reservation-")
        .tempfile_in(parent)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    let file = staged
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    file.sync_all()?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn repair_one(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    original: &Source,
    attempted: &mut bool,
) -> Result<bool, String> {
    let _mutation = crate::ArchiveMutationGuard::begin_at(&config.storage_root);
    let archive = crate::ensure_archive(config, &original.slug).map_err(source_error)?;
    crate::with_project_lock(&archive, || {
        if read_source(cx, pool, original.id)
            .map_err(invalid)?
            .as_ref()
            != Some(original)
        {
            return Err(invalid(
                "active reservation source changed before publication",
            ));
        }
        let root = crate::archive_project_root_checked(&archive)?;
        let repo_root = crate::archive_repo_root_checked(&archive)?;
        let repo = Repository::open(repo_root)?;
        let tree = head_tree(&repo)?;
        let project_path = root.join("project.json");
        let project_rel = crate::rel_path_cached(&archive.canonical_repo_root, &project_path)?;
        for artifact in [
            read_artifact(&project_path)?,
            committed_artifact(&repo, tree.as_ref(), &project_rel)?,
        ]
        .into_iter()
        .flatten()
        {
            if artifact.value["slug"] != original.slug
                || artifact.value["human_key"] != original.artifact["project"]
            {
                return Err(invalid(
                    "archived project conflicts with active reservation source",
                ));
            }
        }
        let path = root
            .join("file_reservations")
            .join(reservation_artifact_filename(
                Some(&original.generation),
                original.id,
            ));
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, &path)?;
        let working = read_artifact(&path)?;
        let committed = committed_artifact(&repo, tree.as_ref(), &relative)?;
        for artifact in [working.as_ref(), committed.as_ref()].into_iter().flatten() {
            validate_evidence(original, &artifact.value)?;
        }
        let bytes = if let Some(working) = &working {
            // Existing files are never rewritten, even to extend an old expiry.
            for field in ["expires_ts", "reason"] {
                if working.value[field] != original.artifact[field] {
                    return Err(invalid(
                        "existing active artifact differs; create-only repair refused",
                    ));
                }
            }
            working.bytes.clone()
        } else {
            let mut value = committed
                .as_ref()
                .map_or_else(|| json!({}), |a| a.value.clone());
            let object = value.as_object_mut().expect("validated artifact object");
            // Source absence is authoritative. Do not retain a historical
            // nonpositive release spelling that another reader could misread.
            object.remove("released_ts");
            object.extend(
                original
                    .artifact
                    .as_object()
                    .expect("source object")
                    .clone(),
            );
            serde_json::to_vec_pretty(&value)?
        };
        if bytes.len() > ARTIFACT_BYTES {
            return Err(invalid("active reservation artifact exceeds byte bound"));
        }
        if working.is_some() && committed.as_ref().is_some_and(|a| a.bytes == bytes) {
            return Ok(false);
        }
        if read_source(cx, pool, original.id)
            .map_err(invalid)?
            .as_ref()
            != Some(original)
        {
            return Err(invalid(
                "active reservation source changed during verification",
            ));
        }
        *attempted = true;
        if working.is_none() {
            publish_missing(&path, &bytes)?;
        }
        // The commit helper reads the current file, not the captured ACTIVE
        // payload. A racing terminal writer can therefore only supersede it;
        // no stale active WriteOp is ever queued for a later overwrite.
        crate::commit_paths_with_retry(
            repo_root,
            config,
            &format!("repair: missing active reservation {}", original.id),
            &[relative.as_str()],
        )?;
        let verified = Repository::open(repo_root)?;
        let tree = head_tree(&verified)?
            .ok_or_else(|| invalid("active reservation commit has no HEAD"))?;
        let entry = tree.get_path(Path::new(&relative))?;
        if entry.kind() != Some(ObjectType::Blob)
            || !matches!(entry.filemode(), 0o100644 | 0o100755)
            || entry.id()
                != Oid::hash_object_ext(ObjectType::Blob, &bytes, entry.id().object_format())?
            || verified.find_blob(entry.id())?.content() != bytes
            || read_artifact(&path)?.is_none_or(|a| a.bytes != bytes)
        {
            return Err(invalid(
                "active reservation changed during commit; current evidence retained",
            ));
        }
        Ok(true)
    })
    .map_err(|error| error.to_string())
}

/// Repair missing generation-stamped stable artifacts for settled active leases.
/// The pass never modifies database rows or overwrites an occupied destination.
/// Thirty-second-old grants are eligible; expired, released, unversioned and
/// ambiguous sources remain untouched. Row selection and writes are bounded,
/// but this is not a hard deadline on SQLite, Git or filesystem operations.
pub fn reconcile_active_reservations(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ActiveReservationCursor,
    stop: &AtomicBool,
) -> Result<ActiveReservationReport, String> {
    let mut report = ActiveReservationReport::default();
    if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
        report.interrupted = true;
        return Ok(report);
    }
    let _activity = mcp_agent_mail_db::write_barrier::begin_write_activity();
    if corruption_circuit_breaker().is_tripped() {
        return Err("active reservation repair refused: corruption breaker open".into());
    }
    validate_source(cx, pool, config)?;
    let identity = pool.sqlite_identity_key();
    if cursor.identity != identity {
        *cursor = ActiveReservationCursor {
            identity,
            ..Default::default()
        };
    }
    let conn = outcome(block_on(pool.acquire(cx)))?;
    if cursor.ceiling.is_none() {
        let rows = conn
            .query_sync("SELECT COALESCE(MAX(id), 0) FROM file_reservations", &[])
            .map_err(source_error)?;
        cursor.ceiling = Some(
            rows.first()
                .ok_or("reservation ID aggregate missing")?
                .get_as::<i64>(0)
                .map_err(source_error)?,
        );
        cursor.after = 0;
    }
    let rows = conn.query_sync(
        "SELECT id FROM file_reservations WHERE id > ? AND id <= ? AND expires_ts > ? \
         AND created_ts <= ? AND (released_ts IS NULL OR released_ts <= 0) \
         AND NOT EXISTS (SELECT 1 FROM file_reservation_releases rr WHERE rr.reservation_id = file_reservations.id) \
         ORDER BY id LIMIT ?",
        &[cursor.after.into(), cursor.ceiling.unwrap_or(0).into(),
          mcp_agent_mail_db::now_micros().into(),
          mcp_agent_mail_db::now_micros().saturating_sub(GRANT_GRACE_US).into(), IDS_PER_PASS.into()],
    ).map_err(source_error)?;
    let ids = rows
        .iter()
        .map(|row| row.get_named::<i64>("id").map_err(source_error))
        .collect::<Result<Vec<_>, _>>()?;
    drop(conn);
    report.more = ids.len() == usize::try_from(IDS_PER_PASS).expect("small ID bound");
    for id in ids {
        if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
            report.interrupted = true;
            report.more = true;
            break;
        }
        if report.attempted >= WRITES_PER_PASS {
            report.more = true;
            break;
        }
        if corruption_circuit_breaker().is_tripped() {
            return Err("active reservation repair stopped: source corruption observed".into());
        }
        let mut attempted = false;
        let result = read_source(cx, pool, id).and_then(|source| {
            source.map_or(Ok(false), |source| {
                repair_one(cx, pool, config, &source, &mut attempted)
            })
        });
        report.scanned += 1;
        report.attempted += usize::from(attempted);
        match result {
            Ok(true) => report.repaired += 1,
            Ok(false) => report.unchanged += 1,
            Err(reason) => {
                report.deferred += 1;
                tracing::warn!(target: "maintenance", reservation_id = id, %reason,
                    "active reservation repair deferred; existing evidence preserved");
            }
        }
        cursor.after = id;
    }
    if !report.more && !report.interrupted {
        cursor.ceiling = None;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(test: impl FnOnce(&Cx, &DbPool, &Config)) {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let config = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                    &temp.path().join("mail.sqlite3"),
                ),
                storage_root: temp.path().join("archive"),
                ..Default::default()
            };
            fs::create_dir_all(&config.storage_root).unwrap();
            let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1)").unwrap();
            conn.execute_raw(
                "INSERT OR REPLACE INTO db_identity(singleton, generation_id) VALUES(0, 'aabb')",
            )
            .unwrap();
            let expires = mcp_agent_mail_db::now_micros() + 3_600_000_000;
            conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES(401, 71, 81, 'src/*.rs', 1, 'active-test', 1000000, {expires}, NULL)")).unwrap();
            drop(conn);
            test(&cx, &pool, &config);
        });
    }

    fn target(config: &Config) -> std::path::PathBuf {
        config
            .storage_root
            .join("projects/project/file_reservations/id-401-gaabb.json")
    }

    fn pass(cx: &Cx, pool: &DbPool, config: &Config) -> ActiveReservationReport {
        reconcile_active_reservations(
            cx,
            pool,
            config,
            &mut ActiveReservationCursor::default(),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    #[test]
    fn missing_active_lease_is_committed_without_mutating_the_database_or_aliases() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let archive = crate::ensure_archive(config, &source.slug).unwrap();
            // Existing aliases and other generations are not targets of repair.
            let foreign = archive.root.join("file_reservations/id-401-gccdd.json");
            crate::ensure_parent_dir(&foreign).unwrap();
            fs::write(&foreign, b"retained foreign-generation evidence").unwrap();
            let report = pass(cx, pool, config);
            assert_eq!(
                (report.scanned, report.repaired, report.deferred),
                (1, 1, 0)
            );
            let path = target(config);
            assert_eq!(
                read_artifact(&path).unwrap().unwrap().value,
                source.artifact
            );
            assert_eq!(
                fs::read(&foreign).unwrap(),
                b"retained foreign-generation evidence"
            );
            let files = fs::read_dir(path.parent().unwrap()).unwrap().count();
            assert_eq!(files, 2, "no digest or legacy alias is manufactured");
            let repo = Repository::open(&config.storage_root).unwrap();
            let head = repo.head().unwrap().target().unwrap();
            let relative = crate::rel_path_cached(&archive.canonical_repo_root, &path).unwrap();
            let tree = head_tree(&repo).unwrap().unwrap();
            assert_eq!(
                committed_artifact(&repo, Some(&tree), &relative)
                    .unwrap()
                    .unwrap()
                    .value,
                source.artifact
            );
            assert_eq!(read_source(cx, pool, 401).unwrap().unwrap(), source);
            let repeated = pass(cx, pool, config);
            assert_eq!((repeated.repaired, repeated.unchanged), (0, 1));
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
        });
    }

    #[test]
    fn released_expired_and_in_flight_grants_are_not_recreated() {
        for change in [
            "UPDATE file_reservations SET released_ts=5000000 WHERE id=401".to_string(),
            "INSERT INTO file_reservation_releases(reservation_id, released_ts) VALUES(401, 5000000)".to_string(),
            "UPDATE file_reservations SET expires_ts=9000000 WHERE id=401".to_string(),
            format!("UPDATE file_reservations SET created_ts={} WHERE id=401", mcp_agent_mail_db::now_micros()),
        ] {
            fixture(|cx, pool, config| {
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                conn.execute_raw(&change).unwrap();
                drop(conn);
                let report = pass(cx, pool, config);
                assert_eq!(report.repaired, 0, "{change}");
                assert!(!target(config).exists());
            });
        }
    }

    #[test]
    fn captured_active_state_cannot_override_a_release_renewal_or_new_generation() {
        for change in [
            "INSERT INTO file_reservation_releases(reservation_id, released_ts) VALUES(401, 5000000)",
            "UPDATE file_reservations SET expires_ts=expires_ts+1000000 WHERE id=401",
            "UPDATE db_identity SET generation_id='ccdd' WHERE singleton=0",
        ] {
            fixture(|cx, pool, config| {
                let captured = read_source(cx, pool, 401).unwrap().unwrap();
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                conn.execute_raw(change).unwrap();
                drop(conn);
                let mut attempted = false;
                assert!(repair_one(cx, pool, config, &captured, &mut attempted).is_err());
                assert!(!attempted);
                assert!(!target(config).exists());
            });
        }
    }

    #[test]
    fn committed_terminal_evidence_survives_worktree_loss() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let archive = crate::ensure_archive(config, &source.slug).unwrap();
            let path = target(config);
            crate::ensure_parent_dir(&path).unwrap();
            let mut terminal = source.artifact.clone();
            terminal["released_ts"] = json!("1970-01-01T00:00:05Z");
            let bytes = serde_json::to_vec_pretty(&terminal).unwrap();
            fs::write(&path, &bytes).unwrap();
            let relative = crate::rel_path_cached(&archive.canonical_repo_root, &path).unwrap();
            crate::commit_paths_with_retry(
                &archive.repo_root,
                config,
                "fixture: release",
                &[&relative],
            )
            .unwrap();
            let retained = config.storage_root.join("retained-terminal.json");
            fs::rename(&path, &retained).unwrap();
            let repo = Repository::open(&archive.repo_root).unwrap();
            let head = repo.head().unwrap().target().unwrap();
            let report = pass(cx, pool, config);
            assert_eq!((report.repaired, report.deferred), (0, 1));
            assert!(!path.exists());
            assert_eq!(fs::read(retained).unwrap(), bytes);
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
        });
    }

    #[test]
    fn create_only_publication_cannot_clobber_a_racing_terminal_writer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease.json");
        let terminal = b"{\"released_ts\": 123}";
        // The writer arrives after the repair's earlier absence observation.
        assert!(!path.exists());
        fs::write(&path, terminal).unwrap();
        assert!(publish_missing(&path, b"{\"released_ts\": null}").is_err());
        assert_eq!(fs::read(path).unwrap(), terminal);
    }

    #[test]
    fn interrupted_publication_is_committed_but_existing_expiry_is_never_rewritten() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            crate::ensure_archive(config, &source.slug).unwrap();
            let path = target(config);
            let mut value = source.artifact.clone();
            value["operator_note"] = json!("retain exact surviving formatting");
            let bytes = serde_json::to_vec(&value).unwrap();
            publish_missing(&path, &bytes).unwrap();
            assert_eq!(pass(cx, pool, config).repaired, 1);
            assert_eq!(fs::read(&path).unwrap(), bytes);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "UPDATE file_reservations SET expires_ts=expires_ts+1000000 WHERE id=401",
            )
            .unwrap();
            drop(conn);
            let report = pass(cx, pool, config);
            assert_eq!((report.repaired, report.deferred), (0, 1));
            assert_eq!(fs::read(path).unwrap(), bytes);
        });
    }

    #[test]
    fn bounded_round_progresses_past_conflicts_and_defers_new_arrivals_to_next_round() {
        fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 402..411 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) SELECT {id}, project_id, agent_id, 'src/{id}.rs', \"exclusive\", reason, created_ts, expires_ts, NULL FROM file_reservations WHERE id=401")).unwrap();
            }
            drop(conn);
            let path = target(config);
            crate::ensure_parent_dir(&path).unwrap();
            fs::write(&path, b"malformed retained evidence").unwrap();
            let mut cursor = ActiveReservationCursor::default();
            let first = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!((first.repaired, first.deferred), (WRITES_PER_PASS, 1));
            assert!(first.more);
            let ceiling = cursor.ceiling;
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) SELECT 900, project_id, agent_id, 'new.rs', \"exclusive\", reason, created_ts, expires_ts, NULL FROM file_reservations WHERE id=401").unwrap();
            drop(conn);
            let second = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(second.repaired, WRITES_PER_PASS);
            assert_eq!(cursor.ceiling, ceiling);
            assert!(cursor.after < 900);
            assert_eq!(fs::read(path).unwrap(), b"malformed retained evidence");
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_destination_is_preserved_and_does_not_redirect_repair() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            crate::ensure_archive(config, &source.slug).unwrap();
            let path = target(config);
            crate::ensure_parent_dir(&path).unwrap();
            let outside = config.storage_root.join("outside.json");
            fs::write(&outside, b"outside evidence").unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
            let report = pass(cx, pool, config);
            assert_eq!((report.repaired, report.deferred), (0, 1));
            assert_eq!(fs::read_link(&path).unwrap(), outside);
            assert_eq!(fs::read(outside).unwrap(), b"outside evidence");
        });
    }

    #[test]
    fn shutdown_and_foreign_mailbox_binding_do_not_authorize_writes() {
        fixture(|cx, pool, config| {
            let mut cursor = ActiveReservationCursor::default();
            let report = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(true),
            )
            .unwrap();
            assert!(report.interrupted);
            assert_eq!(report.scanned, 0);
            assert!(!target(config).exists());
            let other = config.storage_root.join("other.sqlite3");
            fs::write(&other, b"not this mailbox").unwrap();
            let foreign = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&other),
                ..config.clone()
            };
            assert!(
                reconcile_active_reservations(
                    cx,
                    pool,
                    &foreign,
                    &mut cursor,
                    &AtomicBool::new(false)
                )
                .is_err()
            );
            assert!(!target(config).exists());
            assert_eq!(fs::read(other).unwrap(), b"not this mailbox");
        });
    }
}
