//! Recovery helpers for repos damaged by the git 2.51.0 index-race.
//!
//! See `docs/GIT_251_FINDINGS.md` for background on the bug and
//! `docs/RECOVERY_RUNBOOK.md` for the operator playbook.
//!
//! # What this module does
//!
//! - [`detect_missing_refs`] (br-8ujfs.6.2 / F2): walk every ref in a
//!   repo and identify ones whose target object is missing from the
//!   object database. These are "orphan" refs left behind when a
//!   writer crashed mid-update.
//! - [`prune_missing_ref`] (F3): revalidate a finding under the Git ref
//!   lock before pruning; callers retain backup and repository-lock ownership.
//! - [`message_reconcile`] (br-8j6cb): restore missing message artifacts
//!   without overwriting conflicting evidence or delivering mail again.
//!
//! # Ref-detection non-goals
//!
//! - Ref detection NEVER touches the working tree. Its operations are pure
//!   ref / ODB introspection; message reconciliation is a separate write API.
//! - We NEVER delete objects. If a ref points to a missing object we
//!   delete the REF; the (missing) object is already gone.
//! - Ref detection NEVER auto-repairs. Detection is strictly read-only; the
//!   caller (Track F's `am doctor fix-orphan-refs` command) decides
//!   when to prune.

pub mod active_reservations;
pub mod agent_reconcile;
pub mod message_reconcile;
pub mod ref_backup;
pub mod reservation_reconcile;

use std::path::Path;

use git2::{ErrorCode, ObjectType, Oid, Repository};

/// A reservation and both release authorities from one database snapshot.
///
/// Keep the point lookups inside their own SELECTs. In FrankenSQLite 0.4.13,
/// the cross-table byte predicate prevents the outer WHERE from narrowing a
/// joined table, making an otherwise single-ID read scan reservation history
/// (GH #354). Both growing tables must be narrowed before joining identities.
/// The outer predicate still rejects oversized source values before they cross
/// the connection boundary; neither the byte limit nor release authority is
/// inferred from a separately observed row.
pub(super) const RESERVATION_SOURCE_SQL: &str = "SELECT fr.id, fr.project_id, fr.agent_id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
            fr.created_ts, fr.expires_ts, CAST(fr.released_ts AS TEXT) AS hot_release, \
            rr.released_ts AS ledger_release, p.slug, p.human_key, a.name, d.generation_id \
     FROM (SELECT id, project_id, agent_id, path_pattern, \"exclusive\", reason, \
                  created_ts, expires_ts, released_ts \
           FROM file_reservations WHERE id = ?1) fr \
     JOIN projects p ON p.id = fr.project_id \
     JOIN agents a ON a.id = fr.agent_id AND a.project_id = fr.project_id \
     LEFT JOIN (SELECT reservation_id, released_ts \
                FROM file_reservation_releases WHERE reservation_id = ?1) rr \
       ON rr.reservation_id = fr.id \
     LEFT JOIN db_identity d ON d.singleton = 0 \
     WHERE length(CAST(fr.path_pattern AS BLOB)) + length(CAST(fr.reason AS BLOB)) + \
           length(CAST(p.slug AS BLOB)) + length(CAST(p.human_key AS BLOB)) + length(CAST(a.name AS BLOB)) + \
           length(CAST(fr.created_ts AS BLOB)) + length(CAST(fr.expires_ts AS BLOB)) + \
           length(CAST(fr.\"exclusive\" AS BLOB)) + \
           COALESCE(length(CAST(fr.released_ts AS BLOB)), 0) + \
           COALESCE(length(CAST(rr.released_ts AS BLOB)), 0) + \
           COALESCE(length(CAST(d.generation_id AS BLOB)), 0) <= ?2";

fn query_reservation_source(
    conn: &mcp_agent_mail_db::DbConn,
    id: i64,
    max_source_bytes: i64,
) -> Result<Vec<mcp_agent_mail_db::sqlmodel_core::Row>, mcp_agent_mail_db::sqlmodel_core::Error> {
    #[cfg(test)]
    tests::record_reservation_source_query();
    conn.query_sync(
        RESERVATION_SOURCE_SQL,
        &[id.into(), max_source_bytes.into()],
    )
}

/// A ref that cannot be followed because its target object is missing
/// from the repository's object database.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PrunableRef {
    /// Full ref name, e.g. `refs/stash`, `refs/heads/foo`.
    pub ref_name: String,

    /// The original direct target OID, not a peeled tag-chain target.
    /// Locked pruning compares this exact identity before removing the ref.
    pub target_sha: String,

    /// Short human-readable reason, included in the action log.
    pub reason: String,

    /// True if this ref is in a namespace we consider SAFE to prune
    /// without operator override. See [`ref_category`].
    pub category: RefCategory,
}

/// Classification of refs for the pruning safety gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RefCategory {
    /// Primary refs (main/master/HEAD). Never auto-prune even with
    /// `--force`; operator must intervene manually.
    Protected,
    /// Safe-to-prune namespaces (`refs/stash`, `refs/temp/*`,
    /// `refs/original/*`).
    SafeToPrune,
    /// Everything else (`refs/heads/*`, `refs/tags/*`,
    /// `refs/remotes/*`, custom). Requires `--force`.
    AskUser,
}

/// Classify a ref name into a [`RefCategory`].
///
/// This is the central safety gate for F3 (pruning). Called by both
/// detection (to label findings) and the prune path (to decide whether
/// to proceed).
#[must_use]
pub fn ref_category(ref_name: &str) -> RefCategory {
    // Protected: primary branches + HEAD + their remote tracking.
    const PROTECTED: &[&str] = &[
        "HEAD",
        "refs/heads/main",
        "refs/heads/master",
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
        "refs/remotes/origin/HEAD",
    ];
    if PROTECTED.contains(&ref_name) {
        return RefCategory::Protected;
    }

    // Safe-to-prune namespaces.
    //
    // Note: `refs/stash` is a LEAF ref (git stores multiple stashes as a
    // reflog on a single ref tip, not as a namespace), so it's matched
    // exactly rather than via starts_with — otherwise `refs/stashy/foo`
    // would also match, which is wrong.
    if ref_name == "refs/stash" {
        return RefCategory::SafeToPrune;
    }
    const SAFE_PREFIXES: &[&str] = &["refs/temp/", "refs/original/"];
    for prefix in SAFE_PREFIXES {
        if ref_name.starts_with(prefix) {
            return RefCategory::SafeToPrune;
        }
    }

    RefCategory::AskUser
}

/// Result of revalidating an orphan finding at the mutation boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneRefOutcome {
    /// The same direct ref still named a missing object and was removed.
    Pruned,
    /// Another operation already removed the ref.
    AlreadyAbsent,
    /// The ref now has another target or is symbolic. It was not changed.
    Changed,
    /// The original object is now available. Its ref was not changed.
    TargetPresent,
}

/// Prune only the exact, still-missing direct ref described by a finding.
///
/// The caller must obtain its repository coordination lock and successfully
/// write its recovery backup before calling this mutation API. That flock
/// does not serialize ordinary Git clients: a Git ref transaction holds the
/// actual ref lock from revalidation through deletion. Never trust a finding's
/// cached category or delete a ref merely because its name appeared in a scan.
///
/// A changed, symbolic, restored, or already-absent ref is a successful skip,
/// not a deletion. Only `ErrorCode::NotFound` from the ODB proves absence;
/// corruption and I/O errors must not authorize destructive recovery.
///
/// # Errors
///
/// Returns an error for protected refs (even with `force`), an unapproved
/// namespace, malformed OIDs, lock failures, or uncertain repository state.
/// No working-tree files or objects are deleted.
pub fn prune_missing_ref(
    repo_path: &Path,
    finding: &PrunableRef,
    force: bool,
) -> Result<PruneRefOutcome, git2::Error> {
    match ref_category(&finding.ref_name) {
        RefCategory::Protected => {
            return Err(git2::Error::from_str("refusing to prune a protected ref"));
        }
        RefCategory::AskUser if !force => {
            return Err(git2::Error::from_str(
                "refusing to prune an unknown namespace without force",
            ));
        }
        RefCategory::SafeToPrune | RefCategory::AskUser => {}
    }
    let expected = Oid::from_str(&finding.target_sha)?;
    let repo = Repository::open(repo_path)?;
    let mut transaction = repo.transaction()?;
    transaction.lock_ref(&finding.ref_name)?;

    let reference = match repo.find_reference(&finding.ref_name) {
        Ok(reference) => reference,
        Err(error) if error.code() == ErrorCode::NotFound => {
            return Ok(PruneRefOutcome::AlreadyAbsent);
        }
        Err(error) => return Err(error),
    };
    if reference.target() != Some(expected) {
        return Ok(PruneRefOutcome::Changed);
    }
    match repo.odb()?.read_header(expected) {
        Ok(_) => return Ok(PruneRefOutcome::TargetPresent),
        Err(error) if error.code() == ErrorCode::NotFound => {}
        Err(error) => return Err(error),
    }

    transaction.remove(&finding.ref_name)?;
    transaction.commit()?;
    Ok(PruneRefOutcome::Pruned)
}

/// Detect direct refs whose target objects are missing from the repo's ODB.
///
/// This uses libgit2 rather than invoking `git fsck`. A missing direct target
/// is a finding; an unreadable reference/object or an unresolved symbolic/tag
/// chain is an error. Callers must not mistake an incomplete scan for a clean
/// repository or use its partial findings to authorize destructive recovery.
///
/// # Arguments
///
/// - `repo_path`: path to the repo (normal, bare, or worktree).
///
/// # Returns
///
/// One [`PrunableRef`] per confirmed missing direct target. Each finding keeps
/// the original ref OID for mutation-time revalidation. An empty result means
/// that all enumerated refs resolved, their object headers were readable, and
/// any annotated tags peeled successfully. This is not a complete audit of
/// commit/tree reachability, object contents, or pseudorefs such as HEAD.
///
/// # Errors
///
/// Returns an error if the repo, ref iterator, ref name, symbolic resolution,
/// object header, or tag chain cannot be read reliably. Only an ODB
/// `ErrorCode::NotFound` for a direct ref produces a prunable finding.
pub fn detect_missing_refs(repo_path: &Path) -> Result<Vec<PrunableRef>, git2::Error> {
    let repo = Repository::open(repo_path)?;
    let odb = repo.odb()?;
    let mut out = Vec::new();

    for reference in repo.references()? {
        // Never silently drop an iterator error or manufacture a ref name.
        let reference = reference?;
        let name = reference.name()?.to_string();
        let resolved = reference.resolve().map_err(|error| {
            git2::Error::from_str(&format!("cannot resolve reference {name}: {error}"))
        })?;
        let oid = resolved.target().ok_or_else(|| {
            git2::Error::from_str(&format!("reference {name} has no resolved direct target"))
        })?;

        match odb.read_header(oid) {
            Ok((_, kind)) => {
                // A readable tag object is not sufficient: its target chain
                // can still be missing or corrupt. Do not fall back to the
                // tag's own OID after a failed peel and call the ref healthy.
                if kind == ObjectType::Tag {
                    resolved.peel(ObjectType::Any).map_err(|error| {
                        git2::Error::from_str(&format!(
                            "cannot validate tag chain for {name}: {error}"
                        ))
                    })?;
                }
                tracing::trace!(
                    target: "mcp_agent_mail::storage::recovery",
                    ref = %name,
                    oid = %oid,
                    "recovery_ref_target_readable"
                );
            }
            Err(error) if error.code() == ErrorCode::NotFound && reference.target().is_some() => {
                let category = ref_category(&name);
                out.push(PrunableRef {
                    ref_name: name.clone(),
                    target_sha: oid.to_string(),
                    reason: format!("object {oid} missing from ODB (via direct-target)"),
                    category,
                });
                tracing::info!(
                    target: "mcp_agent_mail::storage::recovery",
                    ref = %name,
                    oid = %oid,
                    category = ?category,
                    "recovery_ref_missing_object"
                );
            }
            Err(error) => {
                // Includes symbolic aliases resolving to missing objects.
                // An alias is not authority to prune its target or itself.
                return Err(git2::Error::from_str(&format!(
                    "cannot validate target {oid} for reference {name}: {error}"
                )));
            }
        }
    }

    Ok(out)
}

/// Summary counts for reporting (matches F1's JSON schema skeleton).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct DetectionSummary {
    pub total_refs_scanned: usize,
    pub findings: usize,
    pub by_category: CategoryCounts,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct CategoryCounts {
    pub protected: usize,
    pub safe_to_prune: usize,
    pub ask_user: usize,
}

impl DetectionSummary {
    #[must_use]
    pub fn from_findings(total_scanned: usize, findings: &[PrunableRef]) -> Self {
        let mut by_category = CategoryCounts::default();
        for f in findings {
            match f.category {
                RefCategory::Protected => by_category.protected += 1,
                RefCategory::SafeToPrune => by_category.safe_to_prune += 1,
                RefCategory::AskUser => by_category.ask_user += 1,
            }
        }
        Self {
            total_refs_scanned: total_scanned,
            findings: findings.len(),
            by_category,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;
    use mcp_agent_mail_db::sqlmodel_core::{Row, Value};
    use mcp_agent_mail_db::{CanonicalDbConn, DbConn};
    use std::cell::Cell;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    thread_local! {
        static RESERVATION_SOURCE_QUERIES: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn record_reservation_source_query() {
        RESERVATION_SOURCE_QUERIES.with(|count| count.set(count.get() + 1));
    }

    pub(super) fn reset_reservation_source_queries() {
        RESERVATION_SOURCE_QUERIES.with(|count| count.set(0));
    }

    pub(super) fn reservation_source_queries() -> usize {
        RESERVATION_SOURCE_QUERIES.with(Cell::get)
    }

    /// Fill a real repair fixture with expired, unreleased history. The caller
    /// retains its existing sentinel and selects the few actual repair debts.
    pub(super) fn seed_reservation_history(
        conn: &DbConn,
        project_id: i64,
        agent_id: i64,
        skip_id: i64,
    ) {
        conn.execute_raw("BEGIN IMMEDIATE").unwrap();
        let header = "INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \
                      \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES ";
        let mut rows = Vec::with_capacity(128);
        for id in (1..=9_482).filter(|id| *id != skip_id) {
            rows.push(format!(
                "({id}, {project_id}, {agent_id}, 'history/file-{id}.rs', 1, \
                 'mailbox-scale reservation source regression with realistic historical metadata', \
                 1000000, 9000000, NULL)"
            ));
            if rows.len() == 128 {
                conn.execute_raw(&format!("{header}{}", rows.join(",")))
                    .unwrap();
                rows.clear();
            }
        }
        if !rows.is_empty() {
            conn.execute_raw(&format!("{header}{}", rows.join(",")))
                .unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
    }

    // Retain the actual pre-GH354 query as the independent oracle and benchmark
    // incumbent. Do not derive it by transforming the candidate SQL.
    const LEGACY_RESERVATION_SOURCE_SQL: &str = "SELECT fr.id, fr.project_id, fr.agent_id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                fr.created_ts, fr.expires_ts, CAST(fr.released_ts AS TEXT) AS hot_release, \
                rr.released_ts AS ledger_release, p.slug, p.human_key, a.name, d.generation_id \
         FROM file_reservations fr JOIN projects p ON p.id = fr.project_id \
         JOIN agents a ON a.id = fr.agent_id AND a.project_id = fr.project_id \
         LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
         LEFT JOIN db_identity d ON d.singleton = 0 \
         WHERE fr.id = ?1 AND length(CAST(fr.path_pattern AS BLOB)) + length(CAST(fr.reason AS BLOB)) + \
               length(CAST(p.slug AS BLOB)) + length(CAST(p.human_key AS BLOB)) + length(CAST(a.name AS BLOB)) + \
               length(CAST(fr.created_ts AS BLOB)) + length(CAST(fr.expires_ts AS BLOB)) + \
               length(CAST(fr.\"exclusive\" AS BLOB)) + \
               COALESCE(length(CAST(fr.released_ts AS BLOB)), 0) + \
               COALESCE(length(CAST(rr.released_ts AS BLOB)), 0) + \
               COALESCE(length(CAST(d.generation_id AS BLOB)), 0) <= ?2";

    const RESERVATION_FIXTURE_BYTES: i64 = 32 * 1024;

    fn seed_rows(conn: &CanonicalDbConn, header: &str, values: impl Iterator<Item = String>) {
        let mut batch = Vec::with_capacity(128);
        for value in values {
            batch.push(value);
            if batch.len() == 128 {
                conn.execute_raw(&format!("{header}{}", batch.join(",")))
                    .unwrap();
                batch.clear();
            }
        }
        if !batch.is_empty() {
            conn.execute_raw(&format!("{header}{}", batch.join(",")))
                .unwrap();
        }
    }

    fn reservation_source_mailbox(reservations: i64, messages: i64) -> (TempDir, DbConn) {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("mail.sqlite3");
        // Canonical SQLite only constructs the offline fixture. All measured
        // reads, plans, mutations and snapshot assertions use runtime DbConn.
        let seed = CanonicalDbConn::open_file(path.to_str().unwrap()).unwrap();
        seed.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .unwrap();
        seed.execute_raw("PRAGMA foreign_keys = ON; BEGIN IMMEDIATE")
            .unwrap();
        seed_rows(
            &seed,
            "INSERT INTO projects(id, slug, human_key, created_at) VALUES ",
            (1..=5).map(|id| {
                format!("({id}, 'project-{id}', '/mailbox-scale/project-{id}', 1000000)")
            }),
        );
        seed_rows(
            &seed,
            "INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES ",
            (1..=82).map(|id| {
                let project = (id - 1) % 5 + 1;
                format!("({id}, {project}, 'Agent{id:03}', 'test', 'test', 1000000, 1000000)")
            }),
        );
        seed.execute_raw("INSERT INTO db_identity(singleton, generation_id) VALUES(0, 'aabbccdd')")
            .unwrap();
        seed_rows(
            &seed,
            "INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \
             \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES ",
            (1..=reservations).map(|id| {
                let agent = (id - 1) % 82 + 1;
                let project = (agent - 1) % 5 + 1;
                let hot = if id % 4 == 0 {
                    (1_700_000_001_000_000_i64 + id).to_string()
                } else {
                    "NULL".to_owned()
                };
                format!(
                    "({id}, {project}, {agent}, 'src/project-{project}/file-{id}.rs', {}, \
                     'reservation {id}: retain the holder, release authority and historical repair metadata', \
                     1700000000000000, 1700003600000000, {hot})",
                    id % 2
                )
            }),
        );
        seed_rows(
            &seed,
            "INSERT INTO file_reservation_releases(reservation_id, released_ts) VALUES ",
            (1..=reservations)
                .filter(|id| id % 4 != 3)
                .map(|id| format!("({id}, {})", 1_700_000_002_000_000_i64 + id)),
        );
        let body = "A realistic historical mailbox message with source metadata. ".repeat(20);
        seed_rows(
            &seed,
            "INSERT INTO messages(id, project_id, sender_id, subject, body_md, created_ts) VALUES ",
            (1..=messages).map(|id| {
                let agent = (id - 1) % 82 + 1;
                let project = (agent - 1) % 5 + 1;
                format!(
                    "({id}, {project}, {agent}, 'Historical message {id}', '{body}', {})",
                    1_700_000_000_000_000_i64 + id
                )
            }),
        );
        seed_rows(
            &seed,
            "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES ",
            (1..=messages).map(|id| {
                let sender = (id - 1) % 82 + 1;
                let recipient = if sender + 5 <= 82 {
                    sender + 5
                } else {
                    sender - 5
                };
                format!("({id}, {recipient}, 'to')")
            }),
        );
        seed.execute_raw("COMMIT").unwrap();
        drop(seed);
        let native = DbConn::open_file(path.to_str().unwrap()).unwrap();
        (temp, native)
    }

    fn assert_source_rows_equal(actual: &[Row], expected: &[Row]) {
        assert_eq!(
            actual.len(),
            expected.len(),
            "source row cardinality changed"
        );
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(
                actual.column_names().collect::<Vec<_>>(),
                expected.column_names().collect::<Vec<_>>()
            );
            assert_eq!(
                actual.values().collect::<Vec<_>>(),
                expected.values().collect::<Vec<_>>()
            );
        }
    }

    fn compare_reservation_sources(conn: &DbConn, id: i64, byte_limit: i64) -> Vec<Row> {
        let legacy = conn
            .query_sync(
                LEGACY_RESERVATION_SOURCE_SQL,
                &[id.into(), byte_limit.into()],
            )
            .unwrap();
        let current = query_reservation_source(conn, id, byte_limit).unwrap();
        assert_source_rows_equal(&current, &legacy);
        current
    }

    fn reservation_source_plan(conn: &DbConn, sql: &str) -> Vec<String> {
        conn.query_sync(
            &format!("EXPLAIN QUERY PLAN {sql}"),
            &[4_741.into(), RESERVATION_FIXTURE_BYTES.into()],
        )
        .unwrap()
        .into_iter()
        .map(|row| row.get_as::<String>(3).unwrap())
        .collect()
    }

    fn assert_primary_key_source_seek(conn: &DbConn, table: &str, sql: &str) -> Vec<String> {
        let program = conn
            .query_sync(&format!("EXPLAIN {sql}"), &[4_741.into()])
            .unwrap();
        let opened = program
            .iter()
            .find(|row| {
                row.get_as::<String>(1).unwrap() == "OpenRead"
                    && row
                        .get_as::<String>(5)
                        .is_ok_and(|name| name.eq_ignore_ascii_case(table))
            })
            .unwrap_or_else(|| panic!("no OpenRead for {table}: {program:?}"))
            .get_as::<i64>(2)
            .unwrap();
        assert!(
            program.iter().any(|row| {
                matches!(
                    row.get_as::<String>(1).unwrap().as_str(),
                    "SeekRowid" | "NotExists"
                ) && row.get_as::<i64>(2).unwrap() == opened
            }),
            "{table} point lookup has no primary-key seek: {program:?}"
        );
        assert!(
            !program.iter().any(|row| {
                row.get_as::<String>(1).unwrap() == "Rewind"
                    && row.get_as::<i64>(2).unwrap() == opened
            }),
            "{table} point lookup rewinds reservation history: {program:?}"
        );
        program.iter().map(|row| format!("{row:?}")).collect()
    }

    #[test]
    fn reservation_source_mailbox_scale_matches_legacy_and_seeks() {
        let (_temp, conn) = reservation_source_mailbox(9_482, 8_241);
        for (table, expected) in [
            ("projects", 5),
            ("agents", 82),
            ("messages", 8_241),
            ("file_reservations", 9_482),
        ] {
            assert_eq!(
                conn.query_sync(&format!("SELECT COUNT(*) FROM {table}"), &[])
                    .unwrap()[0]
                    .get_as::<i64>(0)
                    .unwrap(),
                expected
            );
        }
        reset_reservation_source_queries();
        // All four release shapes, different holders/projects, the history
        // tail and absent IDs must preserve every column, not just row counts.
        for id in [1, 2, 3, 4, 81, 82, 401, 4_741, 9_481, 9_482, -1, 9_483] {
            let rows = compare_reservation_sources(&conn, id, RESERVATION_FIXTURE_BYTES);
            assert_eq!(rows.len(), usize::from((1..=9_482).contains(&id)));
        }
        assert!(compare_reservation_sources(&conn, 1, 0).is_empty());
        assert_eq!(reservation_source_queries(), 13);

        // Native EQP is retained verbatim: its JOIN labels are not always the
        // executed access path. Check the emitted programs of the two isolated
        // lookups as well, then measure the complete SQL in the CPU regression.
        let legacy = reservation_source_plan(&conn, LEGACY_RESERVATION_SOURCE_SQL);
        let candidate = reservation_source_plan(&conn, RESERVATION_SOURCE_SQL);
        let reservation_program = assert_primary_key_source_seek(
            &conn,
            "file_reservations",
            "SELECT id, project_id, agent_id, path_pattern, \"exclusive\", reason, \
                    created_ts, expires_ts, released_ts FROM file_reservations WHERE id = ?1",
        );
        let release_program = assert_primary_key_source_seek(
            &conn,
            "file_reservation_releases",
            "SELECT reservation_id, released_ts FROM file_reservation_releases WHERE reservation_id = ?1",
        );
        println!(
            "GH354_RESERVATION_PLANS {}",
            serde_json::json!({
                "backend": std::any::type_name::<DbConn>(),
                "projects": 5, "agents": 82, "messages": 8241, "reservations": 9482,
                "legacy": legacy, "candidate": candidate,
                "reservation_program": reservation_program, "release_program": release_program,
            })
        );
        conn.close_sync().unwrap();
    }

    fn projected_source_bytes(row: &Row) -> usize {
        // An independent byte oracle for the actual projected values. IDs do
        // not belong to the existing text bound; every other source field does.
        row.values()
            .skip(3)
            .map(|value| match value {
                Value::Null => 0,
                Value::Text(text) => text.len(),
                Value::Int(value) => value.to_string().len(),
                Value::BigInt(value) => value.to_string().len(),
                other => panic!("unexpected source value in fixture: {other:?}"),
            })
            .sum()
    }

    #[test]
    fn reservation_source_byte_limit_counts_utf8_and_every_authority() {
        let (_temp, conn) = reservation_source_mailbox(4, 4);
        conn.execute_raw(
            "UPDATE file_reservations SET reason = '', released_ts = 1000000 WHERE id = 1",
        )
        .unwrap();
        let fixed = compare_reservation_sources(&conn, 1, RESERVATION_FIXTURE_BYTES)
            .pop()
            .unwrap();
        let remaining =
            usize::try_from(RESERVATION_FIXTURE_BYTES).unwrap() - projected_source_bytes(&fixed);
        let reason = format!("{}{}", "é".repeat(remaining / 2), "x".repeat(remaining % 2));
        assert_eq!(reason.len(), remaining);
        assert!(reason.chars().count() < reason.len());
        conn.execute_sync(
            "UPDATE file_reservations SET reason = ?1 WHERE id = 1",
            &[Value::Text(reason)],
        )
        .unwrap();
        let exact = compare_reservation_sources(&conn, 1, RESERVATION_FIXTURE_BYTES)
            .pop()
            .unwrap();
        assert_eq!(projected_source_bytes(&exact), 32_768);
        assert!(compare_reservation_sources(&conn, 1, RESERVATION_FIXTURE_BYTES - 1).is_empty());

        // Grow each counted field by exactly one encoded byte. This catches a
        // dropped JOIN contribution as well as a weakened per-table substitute
        // for the combined limit, including both release stores and generation.
        for (column, update) in [
            (
                3,
                "UPDATE file_reservations SET path_pattern = ?1 WHERE id = 1",
            ),
            (
                4,
                "UPDATE file_reservations SET \"exclusive\" = ?1 WHERE id = 1",
            ),
            (5, "UPDATE file_reservations SET reason = ?1 WHERE id = 1"),
            (
                6,
                "UPDATE file_reservations SET created_ts = ?1 WHERE id = 1",
            ),
            (
                7,
                "UPDATE file_reservations SET expires_ts = ?1 WHERE id = 1",
            ),
            (
                8,
                "UPDATE file_reservations SET released_ts = ?1 WHERE id = 1",
            ),
            (
                9,
                "UPDATE file_reservation_releases SET released_ts = ?1 WHERE reservation_id = 1",
            ),
            (10, "UPDATE projects SET slug = ?1 WHERE id = 1"),
            (11, "UPDATE projects SET human_key = ?1 WHERE id = 1"),
            (12, "UPDATE agents SET name = ?1 WHERE id = 1"),
            (
                13,
                "UPDATE db_identity SET generation_id = ?1 WHERE singleton = 0",
            ),
        ] {
            let previous = exact.values().nth(column).unwrap().clone();
            let larger = match &previous {
                Value::Text(text) => Value::Text(format!("{text}a")),
                Value::Int(value) => Value::Int(value.checked_mul(10).unwrap()),
                Value::BigInt(value) => Value::BigInt(value.checked_mul(10).unwrap()),
                other => panic!("unexpected boundary value: {other:?}"),
            };
            conn.execute_sync(update, &[larger]).unwrap();
            assert!(
                compare_reservation_sources(&conn, 1, RESERVATION_FIXTURE_BYTES).is_empty(),
                "field {column} bypassed the combined source-byte limit"
            );
            let admitted = compare_reservation_sources(&conn, 1, RESERVATION_FIXTURE_BYTES + 1);
            assert_eq!(admitted.len(), 1, "field {column} changed row authority");
            assert_eq!(projected_source_bytes(&admitted[0]), 32_769);
            conn.execute_sync(update, &[previous]).unwrap();
        }
        assert_source_rows_equal(
            &compare_reservation_sources(&conn, 1, RESERVATION_FIXTURE_BYTES),
            &[exact],
        );
        conn.close_sync().unwrap();
    }

    #[test]
    fn reservation_source_preserves_missing_and_malformed_authority() {
        let (_temp, conn) = reservation_source_mailbox(4, 4);
        // Deliberately model a damaged/foreign holder; neither query can
        // manufacture authority by joining only on an agent ID.
        conn.execute_raw("PRAGMA foreign_keys = OFF").unwrap();
        for change in [
            "UPDATE file_reservations SET project_id = 999 WHERE id = 1",
            "UPDATE file_reservations SET project_id = 2 WHERE id = 1",
            "UPDATE file_reservations SET project_id = 1, agent_id = 2 WHERE id = 1",
            "UPDATE file_reservations SET agent_id = 999 WHERE id = 1",
        ] {
            conn.execute_raw(change).unwrap();
            assert!(compare_reservation_sources(&conn, 1, RESERVATION_FIXTURE_BYTES).is_empty());
        }
        conn.execute_raw("UPDATE file_reservations SET project_id = 1, agent_id = 1 WHERE id = 1")
            .unwrap();
        // SQL must preserve malformed release/generation evidence for the
        // active/terminal decoders; silently converting it to absence is unsafe.
        conn.execute_raw(
            "UPDATE file_reservations SET released_ts = 'malformed' WHERE id = 1; \
             UPDATE file_reservation_releases SET released_ts = 0 WHERE reservation_id = 1; \
             UPDATE db_identity SET generation_id = '' WHERE singleton = 0",
        )
        .unwrap();
        let malformed = compare_reservation_sources(&conn, 1, RESERVATION_FIXTURE_BYTES);
        assert_eq!(malformed.len(), 1);
        assert_eq!(
            malformed[0].get_named::<String>("hot_release").unwrap(),
            "malformed"
        );
        assert_eq!(malformed[0].get_named::<i64>("ledger_release").unwrap(), 0);
        assert_eq!(
            malformed[0].get_named::<String>("generation_id").unwrap(),
            ""
        );
        conn.execute_raw("DELETE FROM db_identity; DELETE FROM file_reservation_releases WHERE reservation_id = 1")
            .unwrap();
        let absent = compare_reservation_sources(&conn, 1, RESERVATION_FIXTURE_BYTES);
        assert_eq!(absent.len(), 1);
        assert_eq!(
            absent[0]
                .get_named::<Option<String>>("generation_id")
                .unwrap(),
            None
        );
        assert_eq!(
            absent[0]
                .get_named::<Option<i64>>("ledger_release")
                .unwrap(),
            None
        );
        assert_eq!(
            absent[0].get_named::<String>("hot_release").unwrap(),
            "malformed"
        );
        assert!(compare_reservation_sources(&conn, 0, RESERVATION_FIXTURE_BYTES).is_empty());
        conn.close_sync().unwrap();
    }

    #[test]
    fn reservation_source_observations_follow_native_snapshot_boundaries() {
        let (temp, current) = reservation_source_mailbox(4, 4);
        let path = temp.path().join("mail.sqlite3");
        let mode = current
            .query_sync("PRAGMA journal_mode = WAL", &[])
            .unwrap();
        assert_eq!(
            mode[0].get_as::<String>(0).unwrap().to_ascii_lowercase(),
            "wal"
        );
        let pinned = DbConn::open_file(path.to_str().unwrap()).unwrap();
        let writer = DbConn::open_file(path.to_str().unwrap()).unwrap();
        pinned.execute_raw("BEGIN DEFERRED").unwrap();
        let original = query_reservation_source(&pinned, 1, RESERVATION_FIXTURE_BYTES).unwrap();
        assert_eq!(original.len(), 1);
        assert_source_rows_equal(
            &query_reservation_source(&current, 1, RESERVATION_FIXTURE_BYTES).unwrap(),
            &original,
        );
        writer.execute_raw("BEGIN IMMEDIATE").unwrap();
        writer.execute_raw(
            "UPDATE file_reservations SET path_pattern = 'src/after.rs', reason = 'committed', \
                 released_ts = 1700000003000000 WHERE id = 1; \
             UPDATE projects SET slug = 'after-project', human_key = '/mailbox-scale/after' WHERE id = 1; \
             UPDATE agents SET name = 'AfterAgent' WHERE id = 1; \
             UPDATE file_reservation_releases SET released_ts = 1700000004000000 WHERE reservation_id = 1; \
             UPDATE db_identity SET generation_id = '00112233' WHERE singleton = 0",
        )
        .unwrap();
        // An in-flight writer cannot expose an intermediate mix of any of the
        // five tables, even to the autocommit observation used by publication.
        assert_source_rows_equal(
            &query_reservation_source(&current, 1, RESERVATION_FIXTURE_BYTES).unwrap(),
            &original,
        );
        writer.execute_raw("COMMIT").unwrap();
        let mut values = original[0].values().cloned().collect::<Vec<_>>();
        values[3] = Value::Text("src/after.rs".into());
        values[5] = Value::Text("committed".into());
        values[8] = Value::Text("1700000003000000".into());
        values[9] = Value::BigInt(1_700_000_004_000_000);
        values[10] = Value::Text("after-project".into());
        values[11] = Value::Text("/mailbox-scale/after".into());
        values[12] = Value::Text("AfterAgent".into());
        values[13] = Value::Text("00112233".into());
        let committed = vec![Row::new(
            original[0].column_names().map(str::to_owned).collect(),
            values,
        )];
        assert_source_rows_equal(
            &query_reservation_source(&pinned, 1, RESERVATION_FIXTURE_BYTES).unwrap(),
            &original,
        );
        assert_source_rows_equal(
            &query_reservation_source(&current, 1, RESERVATION_FIXTURE_BYTES).unwrap(),
            &committed,
        );
        pinned.execute_raw("COMMIT").unwrap();
        assert_source_rows_equal(
            &query_reservation_source(&pinned, 1, RESERVATION_FIXTURE_BYTES).unwrap(),
            &committed,
        );
        writer
            .execute_raw(
                "BEGIN IMMEDIATE; \
             UPDATE file_reservations SET reason = 'rolled back' WHERE id = 1; \
             UPDATE db_identity SET generation_id = 'ffffffff' WHERE singleton = 0",
            )
            .unwrap();
        assert_source_rows_equal(
            &query_reservation_source(&current, 1, RESERVATION_FIXTURE_BYTES).unwrap(),
            &committed,
        );
        writer.execute_raw("ROLLBACK").unwrap();
        assert_source_rows_equal(
            &query_reservation_source(&current, 1, RESERVATION_FIXTURE_BYTES).unwrap(),
            &committed,
        );
        writer.close_sync().unwrap();
        pinned.close_sync().unwrap();
        current.close_sync().unwrap();
    }

    fn proc_cpu_ticks(path: &Path) -> Option<(String, u64)> {
        let stat = std::fs::read_to_string(path).ok()?;
        let (prefix, fields) = stat.rsplit_once(") ")?;
        let name = prefix.split_once('(')?.1.to_owned();
        let fields = fields.split_whitespace().collect::<Vec<_>>();
        let user = fields.get(11)?.parse::<u64>().ok()?;
        let system = fields.get(12)?.parse::<u64>().ok()?;
        Some((name, user.checked_add(system)?))
    }

    fn reservation_cpu_sample() -> Option<(u64, u64, usize)> {
        let (_, process) = proc_cpu_ticks(Path::new("/proc/self/stat"))?;
        let mut worker_ticks = 0_u64;
        let mut workers = 0;
        for task in std::fs::read_dir("/proc/self/task").ok()? {
            let task = task.ok()?;
            let Some((name, ticks)) = proc_cpu_ticks(&task.path().join("stat")) else {
                continue;
            };
            if name == "fsqlite-worker" {
                workers += 1;
                worker_ticks = worker_ticks.checked_add(ticks)?;
            }
        }
        Some((process, worker_ticks, workers))
    }

    fn run_reservation_cpu_child() -> bool {
        const CHILD_ENV: &str = "AM_GH354_RESERVATION_CPU_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            return false;
        }
        // CPU includes the actual connection worker, so measure in a separate
        // process rather than attributing other parallel tests to this query.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "recovery::tests::reservation_source_mailbox_scale_cpu",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut expired = false;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                expired = true;
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !expired && output.status.success() && stdout.contains("1 passed; 0 failed"),
            "native reservation CPU comparison failed (timeout={expired}): {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        print!("{stdout}");
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        true
    }

    #[test]
    fn reservation_source_mailbox_scale_cpu() {
        if run_reservation_cpu_child() {
            return;
        }
        let (temp, conn) = reservation_source_mailbox(9_482, 8_241);
        let clock_ticks_per_second = std::process::Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|output| output.trim().parse::<u64>().ok());
        // Warm both implementations on the same connection and mailbox before
        // alternating their order. Fixture generation and warmup are excluded.
        assert_eq!(
            compare_reservation_sources(&conn, 1, RESERVATION_FIXTURE_BYTES).len(),
            1
        );
        reset_reservation_source_queries();
        let mut samples = Vec::new();
        let mut wall_us = [0_u64; 2];
        let mut process_ticks = [0_u64; 2];
        let mut worker_ticks = [0_u64; 2];
        let mut cpu_samples = [0_usize; 2];
        for round in 0..16 {
            let id = [1, 401, 4_741, 9_482][round % 4];
            let order = if round % 2 == 0 { [0, 1] } else { [1, 0] };
            let mut results = [None, None];
            for lane in order {
                let before = reservation_cpu_sample();
                let start = Instant::now();
                let rows = if lane == 0 {
                    conn.query_sync(
                        LEGACY_RESERVATION_SOURCE_SQL,
                        &[id.into(), RESERVATION_FIXTURE_BYTES.into()],
                    )
                    .unwrap()
                } else {
                    query_reservation_source(&conn, id, RESERVATION_FIXTURE_BYTES).unwrap()
                };
                let elapsed_us = u64::try_from(start.elapsed().as_micros()).unwrap();
                let after = reservation_cpu_sample();
                let cpu = before.zip(after).and_then(|(before, after)| {
                    if before.2 != after.2 || after.2 == 0 {
                        return None;
                    }
                    Some((
                        after.0.checked_sub(before.0)?,
                        after.1.checked_sub(before.1)?,
                    ))
                });
                wall_us[lane] += elapsed_us;
                if let Some((process, worker)) = cpu {
                    process_ticks[lane] += process;
                    worker_ticks[lane] += worker;
                    cpu_samples[lane] += 1;
                }
                samples.push(serde_json::json!({
                    "round": round, "id": id,
                    "query": if lane == 0 { "legacy" } else { "candidate" },
                    "wall_us": elapsed_us,
                    "process_cpu_ticks": cpu.map(|sample| sample.0),
                    "fsqlite_worker_cpu_ticks": cpu.map(|sample| sample.1),
                    "worker_threads": after.map(|sample| sample.2),
                    "rows": rows.len(),
                }));
                results[lane] = Some(rows);
            }
            assert_source_rows_equal(results[1].as_ref().unwrap(), results[0].as_ref().unwrap());
            assert_eq!(results[1].as_ref().unwrap().len(), 1);
        }
        assert_eq!(reservation_source_queries(), 16);
        println!(
            "GH354_RESERVATION_CPU {}",
            serde_json::json!({
                "scope": "isolated native query comparison; not a running-server idle-CPU measurement",
                "backend": std::any::type_name::<DbConn>(),
                "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
                "projects": 5, "agents": 82, "messages": 8241, "reservations": 9482,
                "database_bytes": std::fs::metadata(temp.path().join("mail.sqlite3")).unwrap().len(),
                "queries_per_variant": 16, "candidate_helper_calls": reservation_source_queries(),
                "clock_ticks_per_second": clock_ticks_per_second,
                "legacy_wall_us": wall_us[0], "candidate_wall_us": wall_us[1],
                "legacy_process_cpu_ticks": process_ticks[0],
                "candidate_process_cpu_ticks": process_ticks[1],
                "legacy_worker_cpu_ticks": worker_ticks[0],
                "candidate_worker_cpu_ticks": worker_ticks[1],
                "cpu_samples_per_variant": cpu_samples, "samples": samples,
            })
        );
        // CPU and elapsed timings are retained evidence, not flaky CI gates.
        // Row equality, byte bounds, native seeks and publication convergence
        // are enforced separately on the same production lookup.
        conn.close_sync().unwrap();
    }

    fn init_repo_with_commit(dir: &Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "recovery-test").unwrap();
        cfg.set_str("user.email", "recovery@local").unwrap();
        // Make an initial commit so we have a real oid to play with.
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let sig = Signature::now("r", "r@local").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("a.txt")).unwrap();
        idx.write().unwrap();
        let tree_oid = idx.write_tree().unwrap();
        {
            let tree = repo.find_tree(tree_oid).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();
        }
        repo
    }

    #[test]
    fn clean_repo_has_no_findings() {
        let tmp = TempDir::new().unwrap();
        let _repo = init_repo_with_commit(tmp.path());
        let findings = detect_missing_refs(tmp.path()).unwrap();
        assert!(
            findings.is_empty(),
            "clean repo should have no findings, got {findings:?}"
        );
    }

    #[test]
    fn detects_orphan_stash_ref() {
        let tmp = TempDir::new().unwrap();
        let _repo = init_repo_with_commit(tmp.path());
        // Write a stash ref pointing to a fake oid.
        let fake = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        std::fs::write(tmp.path().join(".git/refs/stash"), format!("{fake}\n")).unwrap();

        let findings = detect_missing_refs(tmp.path()).unwrap();
        assert_eq!(findings.len(), 1, "expected 1 finding, got {findings:?}");
        let f = &findings[0];
        assert_eq!(f.ref_name, "refs/stash");
        assert_eq!(f.target_sha, fake);
        assert_eq!(f.category, RefCategory::SafeToPrune);
    }

    #[test]
    fn detects_orphan_branch_ref() {
        let tmp = TempDir::new().unwrap();
        let _repo = init_repo_with_commit(tmp.path());
        let fake = "cafebabecafebabecafebabecafebabecafebabe";
        // Write a dangling branch ref.
        std::fs::write(
            tmp.path().join(".git/refs/heads/crash-recovery"),
            format!("{fake}\n"),
        )
        .unwrap();

        let findings = detect_missing_refs(tmp.path()).unwrap();
        let crash = findings
            .iter()
            .find(|f| f.ref_name == "refs/heads/crash-recovery")
            .expect("crash ref flagged");
        assert_eq!(crash.target_sha, fake);
        assert_eq!(crash.category, RefCategory::AskUser);
    }

    #[test]
    fn refuses_to_prune_main_without_force() {
        // main gets Protected classification — downstream prune path
        // will refuse regardless of --force (see F3).
        assert_eq!(ref_category("refs/heads/main"), RefCategory::Protected);
        assert_eq!(ref_category("refs/heads/master"), RefCategory::Protected);
        assert_eq!(ref_category("HEAD"), RefCategory::Protected);
    }

    #[test]
    fn ref_category_recognizes_safe_namespaces() {
        assert_eq!(ref_category("refs/stash"), RefCategory::SafeToPrune);
        assert_eq!(ref_category("refs/temp/foo"), RefCategory::SafeToPrune);
        assert_eq!(
            ref_category("refs/original/refs/heads/foo"),
            RefCategory::SafeToPrune
        );
    }

    #[test]
    fn ref_category_ask_user_default() {
        assert_eq!(
            ref_category("refs/heads/feature-branch"),
            RefCategory::AskUser
        );
        assert_eq!(ref_category("refs/tags/v1.0"), RefCategory::AskUser);
        assert_eq!(ref_category("refs/notes/commits"), RefCategory::AskUser);
    }

    #[test]
    fn ref_category_refs_stash_is_leaf_not_prefix() {
        // `refs/stash` is a SINGLE ref (git stores multiple stashes as a
        // reflog on a single ref tip, not as a namespace), so it's matched
        // exactly rather than via starts_with — otherwise `refs/stashy/*`
        // would also match, which is wrong.
        assert_eq!(ref_category("refs/stash"), RefCategory::SafeToPrune);
        assert_eq!(ref_category("refs/stashy/foo"), RefCategory::AskUser);
        assert_eq!(ref_category("refs/stash-backup"), RefCategory::AskUser);
    }

    #[test]
    fn detection_summary_tallies_categories() {
        let findings = vec![
            PrunableRef {
                ref_name: "refs/stash".to_string(),
                target_sha: "aaa".to_string(),
                reason: "x".to_string(),
                category: RefCategory::SafeToPrune,
            },
            PrunableRef {
                ref_name: "refs/heads/foo".to_string(),
                target_sha: "bbb".to_string(),
                reason: "x".to_string(),
                category: RefCategory::AskUser,
            },
            PrunableRef {
                ref_name: "HEAD".to_string(),
                target_sha: "ccc".to_string(),
                reason: "x".to_string(),
                category: RefCategory::Protected,
            },
        ];
        let s = DetectionSummary::from_findings(42, &findings);
        assert_eq!(s.total_refs_scanned, 42);
        assert_eq!(s.findings, 3);
        assert_eq!(s.by_category.safe_to_prune, 1);
        assert_eq!(s.by_category.ask_user, 1);
        assert_eq!(s.by_category.protected, 1);
    }

    #[test]
    fn mixed_repo_classifies_all_findings() {
        let tmp = TempDir::new().unwrap();
        let _repo = init_repo_with_commit(tmp.path());
        // Stash (safe), crash-recovery heads (ask-user).
        std::fs::write(
            tmp.path().join(".git/refs/stash"),
            "0000000000000000000000000000000000000001\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".git/refs/heads/crash-branch"),
            "0000000000000000000000000000000000000002\n",
        )
        .unwrap();

        let findings = detect_missing_refs(tmp.path()).unwrap();
        assert_eq!(findings.len(), 2, "expected 2 findings: {findings:?}");

        let summary = DetectionSummary::from_findings(findings.len(), &findings);
        assert_eq!(summary.by_category.safe_to_prune, 1);
        assert_eq!(summary.by_category.ask_user, 1);
        assert_eq!(summary.by_category.protected, 0);
    }

    fn write_orphan(repo: &Repository, name: &str, oid: Oid) -> PrunableRef {
        let path = repo.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("{oid}\n")).unwrap();
        PrunableRef {
            ref_name: name.to_string(),
            target_sha: oid.to_string(),
            reason: "test missing object".to_string(),
            category: ref_category(name),
        }
    }

    fn missing_oid() -> Oid {
        Oid::from_str("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap()
    }

    #[test]
    fn guarded_prune_removes_only_the_same_missing_ref() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let head = repo.head().unwrap().target().unwrap();
        let finding = write_orphan(&repo, "refs/temp/orphan", missing_oid());
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::Pruned
        );
        assert!(repo.find_reference(&finding.ref_name).is_err());
        assert_eq!(repo.head().unwrap().target(), Some(head));
        assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"hello\n");
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::AlreadyAbsent
        );
    }

    #[test]
    fn guarded_prune_preserves_a_ref_repaired_after_detection() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/repaired", missing_oid());
        let head = repo.head().unwrap().target().unwrap();
        repo.reference(&finding.ref_name, head, true, "concurrent repair")
            .unwrap();
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::Changed
        );
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(head)
        );
    }

    #[test]
    fn guarded_prune_preserves_a_different_missing_target() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/changed", missing_oid());
        let changed = Oid::from_str("cafebabecafebabecafebabecafebabecafebabe").unwrap();
        write_orphan(&repo, &finding.ref_name, changed);
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::Changed
        );
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(changed)
        );
    }

    #[test]
    fn guarded_prune_preserves_an_object_restored_under_the_same_oid() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let bytes = b"object restored after detection";
        let oid = Oid::hash_object(ObjectType::Blob, bytes).unwrap();
        let finding = write_orphan(&repo, "refs/temp/restored", oid);
        assert_eq!(
            repo.odb().unwrap().write(ObjectType::Blob, bytes).unwrap(),
            oid
        );
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::TargetPresent
        );
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(oid)
        );
    }

    #[test]
    fn guarded_prune_preserves_a_ref_changed_to_symbolic() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/symbolic", missing_oid());
        std::fs::write(
            repo.path().join(&finding.ref_name),
            "ref: refs/heads/not-created\n",
        )
        .unwrap();
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::Changed
        );
        assert_eq!(
            repo.find_reference(&finding.ref_name)
                .unwrap()
                .symbolic_target()
                .unwrap(),
            Some("refs/heads/not-created")
        );
    }

    #[test]
    fn guarded_prune_reclassifies_names_instead_of_trusting_findings() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        for name in ["refs/heads/main", "refs/remotes/origin/main"] {
            let mut finding = write_orphan(&repo, name, missing_oid());
            finding.category = RefCategory::SafeToPrune;
            assert!(prune_missing_ref(tmp.path(), &finding, true).is_err());
            assert!(repo.find_reference(name).is_ok());
        }
        let mut finding = write_orphan(&repo, "refs/heads/recovery-topic", missing_oid());
        finding.category = RefCategory::SafeToPrune;
        assert!(prune_missing_ref(tmp.path(), &finding, false).is_err());
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, true).unwrap(),
            PruneRefOutcome::Pruned
        );
    }

    #[test]
    fn guarded_prune_refuses_a_busy_git_ref_lock() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/locked", missing_oid());
        let mut writer = repo.transaction().unwrap();
        writer.lock_ref(&finding.ref_name).unwrap();
        assert!(prune_missing_ref(tmp.path(), &finding, false).is_err());
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(missing_oid())
        );
    }

    #[test]
    fn guarded_prune_refuses_a_corrupt_object_instead_of_treating_it_as_missing() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/corrupt", missing_oid());
        let oid = finding.target_sha.as_str();
        let dir = repo.path().join("objects").join(&oid[..2]);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&oid[2..]), b"not a zlib object").unwrap();
        assert!(prune_missing_ref(tmp.path(), &finding, false).is_err());
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(missing_oid())
        );
    }

    #[test]
    fn detection_rejects_corrupt_object_headers_without_changing_evidence() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/corrupt-scan", missing_oid());
        let oid = finding.target_sha.as_str();
        let dir = repo.path().join("objects").join(&oid[..2]);
        std::fs::create_dir_all(&dir).unwrap();
        let object_path = dir.join(&oid[2..]);
        std::fs::write(&object_path, b"not a zlib object").unwrap();

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert_eq!(std::fs::read(&object_path).unwrap(), b"not a zlib object");
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(missing_oid())
        );
    }

    #[test]
    fn detection_rejects_a_dangling_symbolic_ref() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let path = repo.path().join("refs/heads/alias");
        let bytes = b"ref: refs/heads/nonexistent\n";
        std::fs::write(&path, bytes).unwrap();

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn detection_rejects_a_symbolic_ref_cycle() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        std::fs::write(
            repo.path().join("refs/heads/alias-a"),
            b"ref: refs/heads/alias-b\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("refs/heads/alias-b"),
            b"ref: refs/heads/alias-a\n",
        )
        .unwrap();

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert!(repo.find_reference("refs/heads/alias-a").is_ok());
        assert!(repo.find_reference("refs/heads/alias-b").is_ok());
    }

    #[test]
    fn detection_does_not_turn_an_alias_into_prune_authority() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let finding = write_orphan(&repo, "refs/temp/direct", missing_oid());
        std::fs::write(
            repo.path().join("refs/temp/alias"),
            b"ref: refs/temp/direct\n",
        )
        .unwrap();

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(missing_oid())
        );
    }

    #[test]
    fn detection_rejects_an_existing_tag_with_a_missing_target() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let text = format!(
            "object {}\ntype commit\ntag broken\ntagger Test <test@local> 1700000000 +0000\n\nbroken target\n",
            missing_oid()
        );
        let tag_oid = repo
            .odb()
            .unwrap()
            .write(ObjectType::Tag, text.as_bytes())
            .unwrap();
        let finding = write_orphan(&repo, "refs/temp/broken-tag", tag_oid);

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert_eq!(
            repo.find_reference(&finding.ref_name).unwrap().target(),
            Some(tag_oid)
        );
        assert_eq!(
            prune_missing_ref(tmp.path(), &finding, false).unwrap(),
            PruneRefOutcome::TargetPresent
        );
    }

    #[test]
    fn detection_accepts_valid_symbolic_refs_and_tag_chains() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        let signature = Signature::now("tag-test", "tag@local").unwrap();
        let tag_oid = repo
            .tag("healthy", commit.as_object(), &signature, "healthy", false)
            .unwrap();
        let tag = repo.find_object(tag_oid, Some(ObjectType::Tag)).unwrap();
        repo.tag("nested", &tag, &signature, "nested", false)
            .unwrap();
        std::fs::write(
            repo.path().join("refs/heads/alias"),
            format!("ref: {}\n", head.name().unwrap()),
        )
        .unwrap();

        assert!(detect_missing_refs(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn detection_accepts_an_empty_unborn_repository() {
        let tmp = TempDir::new().unwrap();
        let _repo = Repository::init(tmp.path()).unwrap();
        assert!(detect_missing_refs(tmp.path()).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn detection_refuses_non_utf8_names_instead_of_inventing_a_prunable_name() {
        use std::os::unix::ffi::OsStringExt;

        let tmp = TempDir::new().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let name = std::ffi::OsString::from_vec(b"invalid-\xff".to_vec());
        let path = repo.path().join("refs/heads").join(name);
        let before = format!("{}\n", missing_oid());
        std::fs::write(&path, &before).unwrap();

        assert!(detect_missing_refs(tmp.path()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), before);
    }
}
