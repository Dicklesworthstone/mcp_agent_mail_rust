//! Preserve delivery cursor authority when merging a healthy private salvage
//! image. Archive replay alone cannot recover an issued delivery sequence: its
//! integer cursors carry no generation token. This module deliberately does not
//! claim continuity when the source ledger is unavailable.
//! Baseline-only legacy images with no surviving issuance witness remain
//! readable; they cannot be distinguished from images that lost every witness.

use crate::CanonicalDbConn;
use crate::error::{DbError, DbResult};
use sqlmodel_core::{Row, Value};
use std::collections::{HashMap, HashSet};

const BATCH_ROWS: usize = 1024;

#[cfg(test)]
mod tests;

const ARCHIVE_ONLY_PREDICATE: &str = "NOT EXISTS (SELECT 1 FROM temp.reconstruct_source_deliveries s \
     WHERE s.project_id = c.project_id AND s.message_id = c.message_id \
       AND s.scope = CASE WHEN c.agent_id IS NULL THEN 'project' ELSE 'agent' END \
       AND s.scope_id = COALESCE(c.agent_id, c.project_id))";

fn refusal(detail: impl std::fmt::Display) -> DbError {
    DbError::Sqlite(format!(
        "reconstruct salvage: cannot preserve inbox delivery history: {detail}"
    ))
}

fn rows(conn: &CanonicalDbConn, sql: &str, values: &[Value]) -> DbResult<Vec<Row>> {
    conn.query_sync(sql, values).map_err(refusal)
}

fn execute(conn: &CanonicalDbConn, sql: &str, values: &[Value]) -> DbResult<()> {
    conn.execute_sync(sql, values).map(|_| ()).map_err(refusal)
}

fn integer(row: &Row, column: &str) -> DbResult<i64> {
    row.get_named(column).map_err(refusal)
}

fn text(row: &Row, column: &str) -> DbResult<String> {
    row.get_named(column).map_err(refusal)
}

fn table_present(conn: &CanonicalDbConn, table: &str) -> DbResult<bool> {
    let objects = rows(
        conn,
        "SELECT type FROM sqlite_master WHERE name = ?",
        &[Value::Text(table.to_string())],
    )?;
    if objects.is_empty() {
        return Ok(false);
    }
    if objects.len() != 1 || text(&objects[0], "type")? != "table" {
        return Err(refusal(format!("{table} is not an ordinary table")));
    }
    Ok(true)
}

fn require_columns(conn: &CanonicalDbConn, table: &str, required: &[&str]) -> DbResult<()> {
    // Callers supply only fixed internal table names.
    let columns = rows(conn, &format!("PRAGMA table_info({table})"), &[])?
        .iter()
        .map(|row| text(row, "name"))
        .collect::<DbResult<HashSet<_>>>()?;
    for column in required {
        if !columns.contains(*column) {
            return Err(refusal(format!(
                "{table} is missing required column {column}"
            )));
        }
    }
    Ok(())
}

fn require_injective_identities(map: &HashMap<i64, i64>, kind: &str) -> DbResult<()> {
    let mut targets = HashSet::new();
    for target in map.values() {
        if !targets.insert(*target) {
            return Err(refusal(format!(
                "distinct source {kind} identities collapse onto recovered {kind} {target}"
            )));
        }
    }
    Ok(())
}

fn validate_source_authority(
    source: &CanonicalDbConn,
    has_events: bool,
    has_state: bool,
) -> DbResult<()> {
    // user_version is a format fence, currently 1; v25/v34 are migration
    // identifiers, not PRAGMA versions. Inspect the real migration ledger.
    let version = rows(source, "PRAGMA user_version", &[])?;
    if integer(&version[0], "user_version")? > i64::from(crate::schema::SCHEMA_VERSION) {
        return Err(refusal("source uses an unknown future schema version"));
    }
    let mut expects_events = has_state;
    let mut expects_state = false;
    let mut expects_shared = has_state || table_present(source, "project_mailbox_receipts")?;
    if table_present(source, crate::schema::MIGRATIONS_TABLE_NAME)? {
        require_columns(source, crate::schema::MIGRATIONS_TABLE_NAME, &["id"])?;
        for row in rows(
            source,
            &format!("SELECT id FROM {}", crate::schema::MIGRATIONS_TABLE_NAME),
            &[],
        )? {
            let id = text(&row, "id")?;
            expects_events |= id.starts_with("v25_")
                || id.starts_with("v32_")
                || id.starts_with("v33_")
                || id.starts_with("v34_");
            expects_state |= id.starts_with("v34_");
            expects_shared |=
                id.starts_with("v32_") || id.starts_with("v33_") || id.starts_with("v34_");
        }
    }
    if table_present(source, "sqlite_sequence")? {
        expects_events |= !rows(
            source,
            "SELECT seq FROM sqlite_sequence WHERE name = 'inbox_delivery_events'",
            &[],
        )?
        .is_empty();
    }
    if !has_events {
        // Empty shared tables are part of the static baseline, even before
        // cursor migrations run. Actual post-cursor delivery/receipt rows are
        // stronger evidence that treating this as pre-cursor would reset an
        // established monitor silently.
        for table in [
            "project_mailbox_deliveries",
            "project_mailbox_receipts",
            "message_delivery_signal_receipts",
        ] {
            if table_present(source, table)? {
                expects_events |=
                    !rows(source, &format!("SELECT 1 FROM {table} LIMIT 1"), &[])?.is_empty();
            }
        }
    }
    expects_state |= !rows(
        source,
        "SELECT name FROM sqlite_master WHERE type = 'trigger' AND name IN \
         ('trg_inbox_delivery_cursor_insert', 'trg_inbox_delivery_cursor_delete', \
          'trg_messages_cascade_inbox_delivery_events')",
        &[],
    )?
    .is_empty();
    expects_events |= expects_state
        || !rows(
            source,
            "SELECT name FROM sqlite_master WHERE type = 'trigger' \
         AND name = 'trg_inbox_delivery_events_recipient_insert'",
            &[],
        )?
        .is_empty();
    if expects_events && !has_events {
        return Err(refusal(
            "issued cursor authority survives but its delivery ledger is missing",
        ));
    }
    if expects_state && !has_state {
        return Err(refusal(
            "applied cursor-state authority survives but its state table is missing",
        ));
    }
    if expects_shared && !table_present(source, "project_mailbox_deliveries")? {
        return Err(refusal(
            "shared mailbox authority survives but its delivery table is missing",
        ));
    }
    Ok(())
}

fn batch(
    conn: &CanonicalDbConn,
    select: &str,
    key: &str,
    after: Option<i64>,
) -> DbResult<Vec<Row>> {
    let (predicate, values) = after.map_or_else(
        || (String::new(), Vec::new()),
        |value| (format!(" WHERE {key} > ?"), vec![Value::BigInt(value)]),
    );
    rows(
        conn,
        &format!("{select}{predicate} ORDER BY {key} LIMIT {BATCH_ROWS}"),
        &values,
    )
}

struct IdentityMaps<'a> {
    projects: &'a HashMap<i64, i64>,
    agents: &'a HashMap<i64, i64>,
    messages: &'a HashMap<i64, i64>,
}

fn mapped(map: &HashMap<i64, i64>, source: i64, kind: &str) -> DbResult<i64> {
    map.get(&source)
        .copied()
        .filter(|target| source > 0 && *target > 0)
        .ok_or_else(|| {
            refusal(format!(
                "{kind} {source} has no unambiguous recovered identity; cursor reset authority is required"
            ))
        })
}

fn mapped_delivery(
    target: &CanonicalDbConn,
    identities: &IdentityMaps<'_>,
    project: i64,
    agent: Option<i64>,
    message: i64,
) -> DbResult<(i64, Option<i64>, i64)> {
    let project = mapped(identities.projects, project, "project")?;
    let agent = agent
        .map(|id| mapped(identities.agents, id, "agent"))
        .transpose()?;
    let message = mapped(identities.messages, message, "message")?;
    let owners = rows(
        target,
        "SELECT (SELECT project_id FROM messages WHERE id = ?) AS message_project, \
                (SELECT project_id FROM agents WHERE id = ?) AS agent_project",
        &[
            Value::BigInt(message),
            agent.map_or(Value::Null, Value::BigInt),
        ],
    )?;
    let owner = owners
        .first()
        .ok_or_else(|| refusal("missing mapped owners"))?;
    if owner
        .get_named::<Option<i64>>("message_project")
        .map_err(refusal)?
        != Some(project)
        || (agent.is_some()
            && owner
                .get_named::<Option<i64>>("agent_project")
                .map_err(refusal)?
                != Some(project))
    {
        return Err(refusal("a delivery maps outside its recovered project"));
    }
    Ok((project, agent, message))
}

fn remember_source_delivery(
    target: &CanonicalDbConn,
    project: i64,
    agent: Option<i64>,
    message: i64,
    source_message: i64,
) -> DbResult<()> {
    let scope = if agent.is_some() { "agent" } else { "project" };
    let existing = rows(
        target,
        "SELECT source_message_id FROM temp.reconstruct_source_deliveries \
         WHERE project_id = ? AND scope = ? AND scope_id = ? AND message_id = ?",
        &[
            Value::BigInt(project),
            Value::Text(scope.to_string()),
            Value::BigInt(agent.unwrap_or(project)),
            Value::BigInt(message),
        ],
    )?;
    if let Some(row) = existing.first()
        && integer(row, "source_message_id")? != source_message
    {
        return Err(refusal(
            "distinct source messages collapse onto one recovered delivery identity",
        ));
    }
    execute(
        target,
        "INSERT OR IGNORE INTO temp.reconstruct_source_deliveries \
         (project_id, scope, scope_id, message_id, source_message_id) VALUES (?, ?, ?, ?, ?)",
        &[
            Value::BigInt(project),
            Value::Text(if agent.is_some() { "agent" } else { "project" }.to_string()),
            Value::BigInt(agent.unwrap_or(project)),
            Value::BigInt(message),
            Value::BigInt(source_message),
        ],
    )
}

/// Run only against a private, stable salvage snapshot and a private candidate.
/// All identity maps must be complete. The savepoint makes replacement atomic
/// even for callers already inside the broader salvage transaction.
///
/// State for a removed identity cannot safely be attached to a reused numeric
/// ID. Such sources refuse here until recovery has explicit cursor reset
/// authority; no old cursor is silently reinterpreted as a new position.
/// Returns shared-delivery message IDs whose recipient JSON must be refreshed
/// before subsequent shared receipt restoration and candidate promotion.
pub fn preserve_delivery_history(
    target: &CanonicalDbConn,
    source: &CanonicalDbConn,
    project_ids: &HashMap<i64, i64>,
    agent_ids: &HashMap<i64, i64>,
    message_ids: &HashMap<i64, i64>,
) -> DbResult<Vec<i64>> {
    let has_events = table_present(source, "inbox_delivery_events")?;
    let has_state = table_present(source, "inbox_delivery_cursor_state")?;
    validate_source_authority(source, has_events, has_state)?;
    if !has_events {
        return if has_state {
            Err(refusal(
                "durable cursor state exists without its delivery ledger",
            ))
        } else {
            // Known-old/minimal sources with no migration, sequence, trigger,
            // or state witness have no cursor positions we can preserve.
            Ok(Vec::new())
        };
    }
    require_columns(
        source,
        "inbox_delivery_events",
        &[
            "seq",
            "project_id",
            "agent_id",
            "message_id",
            "kind",
            "delivered_ts",
        ],
    )?;
    if has_state {
        require_columns(
            source,
            "inbox_delivery_cursor_state",
            &[
                "project_id",
                "scope",
                "scope_id",
                "tail_seq",
                "pruned_through_seq",
            ],
        )?;
    }
    require_injective_identities(project_ids, "project")?;
    require_injective_identities(agent_ids, "agent")?;
    let identities = IdentityMaps {
        projects: project_ids,
        agents: agent_ids,
        messages: message_ids,
    };
    target
        .execute_raw("SAVEPOINT reconstruct_delivery_history")
        .map_err(refusal)?;
    let result = preserve_in_savepoint(target, source, &identities, has_state);
    match result {
        Ok(shared_messages) => target
            .execute_raw("RELEASE reconstruct_delivery_history")
            .map(|()| shared_messages)
            .map_err(refusal),
        Err(error) => {
            let _ = target.execute_raw("ROLLBACK TO reconstruct_delivery_history");
            let _ = target.execute_raw("RELEASE reconstruct_delivery_history");
            Err(error)
        }
    }
}

fn source_ceiling(source: &CanonicalDbConn, has_state: bool) -> DbResult<i64> {
    let range = rows(
        source,
        "SELECT MIN(seq) AS first, MAX(seq) AS last FROM inbox_delivery_events",
        &[],
    )?;
    let first = range[0]
        .get_named::<Option<i64>>("first")
        .map_err(refusal)?;
    if first.is_some_and(|seq| seq <= 0) {
        return Err(refusal("delivery sequences must be positive"));
    }
    let mut ceiling = range[0]
        .get_named::<Option<i64>>("last")
        .map_err(refusal)?
        .unwrap_or(0);
    if table_present(source, "sqlite_sequence")? {
        let sequence = rows(
            source,
            "SELECT seq FROM sqlite_sequence WHERE name = 'inbox_delivery_events'",
            &[],
        )?;
        if sequence.len() > 1 {
            return Err(refusal(
                "delivery ledger has ambiguous AUTOINCREMENT authority",
            ));
        }
        if let Some(row) = sequence.first() {
            let issued = integer(row, "seq")?;
            if issued < 0 {
                return Err(refusal("negative delivery AUTOINCREMENT authority"));
            }
            ceiling = ceiling.max(issued);
        }
    }
    if has_state {
        let state = rows(
            source,
            "SELECT COALESCE(MAX(tail_seq), 0) AS tail FROM inbox_delivery_cursor_state",
            &[],
        )?;
        ceiling = ceiling.max(integer(&state[0], "tail")?);
    }
    Ok(ceiling)
}

fn preserve_in_savepoint(
    target: &CanonicalDbConn,
    source: &CanonicalDbConn,
    identities: &IdentityMaps<'_>,
    has_state: bool,
) -> DbResult<Vec<i64>> {
    let ceiling = source_ceiling(source, has_state)?;
    target.execute_raw(
        "CREATE TEMP TABLE reconstruct_candidate_events AS SELECT * FROM main.inbox_delivery_events;
         CREATE TEMP TABLE reconstruct_source_deliveries (
             project_id INTEGER NOT NULL, scope TEXT NOT NULL, scope_id INTEGER NOT NULL,
             message_id INTEGER NOT NULL, source_message_id INTEGER NOT NULL,
             PRIMARY KEY(project_id, scope, scope_id, message_id));",
    ).map_err(refusal)?;

    remember_source_recipients(target, source, identities)?;
    let shared_messages = restore_shared_deliveries(target, source, identities)?;
    // Deletion triggers describe losses in the throwaway replay sequence.
    // Clear those observations before restoring the source's real authority.
    target
        .execute_raw(
            "DELETE FROM inbox_delivery_events;
         DELETE FROM inbox_delivery_cursor_state;
         DELETE FROM sqlite_sequence WHERE name = 'inbox_delivery_events';",
        )
        .map_err(refusal)?;
    execute(
        target,
        "INSERT INTO sqlite_sequence(name, seq) VALUES ('inbox_delivery_events', ?)",
        &[Value::BigInt(ceiling)],
    )?;
    restore_events(target, source, identities, has_state)?;
    restore_cursor_state(target, source, identities, has_state, ceiling)?;

    let additions = rows(
        target,
        &format!(
            "SELECT COUNT(*) AS count FROM temp.reconstruct_candidate_events c WHERE {ARCHIVE_ONLY_PREDICATE}"
        ),
        &[],
    )?;
    ceiling
        .checked_add(integer(&additions[0], "count")?)
        .ok_or_else(|| refusal("delivery sequence exhausted by archive-only additions"))?;
    execute(
        target,
        &format!(
            "INSERT INTO inbox_delivery_events(project_id, agent_id, message_id, kind, delivered_ts) \
         SELECT c.project_id, c.agent_id, c.message_id, c.kind, c.delivered_ts \
         FROM temp.reconstruct_candidate_events c WHERE {ARCHIVE_ONLY_PREDICATE} ORDER BY c.seq"
        ),
        &[],
    )?;
    target
        .execute_raw(
            "DROP TABLE temp.reconstruct_candidate_events;
         DROP TABLE temp.reconstruct_source_deliveries;",
        )
        .map(|()| shared_messages)
        .map_err(refusal)
}

fn remember_source_recipients(
    target: &CanonicalDbConn,
    source: &CanonicalDbConn,
    identities: &IdentityMaps<'_>,
) -> DbResult<()> {
    require_columns(source, "message_recipients", &["message_id", "agent_id"])?;
    let mut after = None;
    loop {
        let page = batch(
            source,
            "SELECT r.rowid AS row_id, r.message_id, r.agent_id, m.project_id \
             FROM message_recipients r LEFT JOIN messages m ON m.id = r.message_id",
            "r.rowid",
            after,
        )?;
        for row in &page {
            after = Some(integer(row, "row_id")?);
            let (project, agent, message) = mapped_delivery(
                target,
                identities,
                integer(row, "project_id")?,
                Some(integer(row, "agent_id")?),
                integer(row, "message_id")?,
            )?;
            remember_source_delivery(target, project, agent, message, integer(row, "message_id")?)?;
        }
        if page.len() < BATCH_ROWS {
            return Ok(());
        }
    }
}

fn restore_shared_deliveries(
    target: &CanonicalDbConn,
    source: &CanonicalDbConn,
    identities: &IdentityMaps<'_>,
) -> DbResult<Vec<i64>> {
    if !table_present(source, "project_mailbox_deliveries")? {
        return Ok(Vec::new());
    }
    require_columns(
        source,
        "project_mailbox_deliveries",
        &["message_id", "project_id", "kind", "delivered_ts"],
    )?;
    let mut after = None;
    let mut shared_messages = Vec::new();
    loop {
        let page = batch(
            source,
            "SELECT message_id, project_id, kind, delivered_ts FROM project_mailbox_deliveries",
            "message_id",
            after,
        )?;
        for row in &page {
            let source_message = integer(row, "message_id")?;
            after = Some(source_message);
            let (project, _, message) = mapped_delivery(
                target,
                identities,
                integer(row, "project_id")?,
                None,
                source_message,
            )?;
            let kind = text(row, "kind")?;
            if !matches!(kind.as_str(), "to" | "cc") {
                return Err(refusal("invalid shared delivery kind"));
            }
            execute(
                target,
                "INSERT INTO project_mailbox_deliveries(message_id, project_id, kind, delivered_ts) VALUES (?, ?, ?, ?) \
                 ON CONFLICT(message_id) DO UPDATE SET project_id = excluded.project_id, kind = excluded.kind, delivered_ts = excluded.delivered_ts",
                &[
                    Value::BigInt(message),
                    Value::BigInt(project),
                    Value::Text(kind),
                    Value::BigInt(integer(row, "delivered_ts")?),
                ],
            )?;
            remember_source_delivery(target, project, None, message, source_message)?;
            shared_messages.push(message);
        }
        if page.len() < BATCH_ROWS {
            return Ok(shared_messages);
        }
    }
}

fn restore_events(
    target: &CanonicalDbConn,
    source: &CanonicalDbConn,
    identities: &IdentityMaps<'_>,
    has_state: bool,
) -> DbResult<()> {
    let mut after = None;
    loop {
        let page = batch(
            source,
            "SELECT seq, project_id, agent_id, message_id, kind, delivered_ts FROM inbox_delivery_events",
            "seq",
            after,
        )?;
        for row in &page {
            let seq = integer(row, "seq")?;
            after = Some(seq);
            let project = integer(row, "project_id")?;
            let raw_agent = row.get_named::<Option<i64>>("agent_id").map_err(refusal)?;
            let kind = text(row, "kind")?;
            let agent = match (kind.as_str(), raw_agent) {
                ("project", None | Some(0)) => None,
                (kind, Some(id)) if kind != "project" && id > 0 => Some(id),
                _ => {
                    return Err(refusal(format!(
                        "delivery {seq} has an invalid recipient scope"
                    )));
                }
            };
            if has_state {
                let state = rows(
                    source,
                    "SELECT tail_seq FROM inbox_delivery_cursor_state WHERE project_id = ? AND scope = ? AND scope_id = ?",
                    &[
                        Value::BigInt(project),
                        Value::Text(if agent.is_some() { "agent" } else { "project" }.to_string()),
                        Value::BigInt(agent.unwrap_or(project)),
                    ],
                )?;
                if state.len() != 1 || integer(&state[0], "tail_seq")? < seq {
                    return Err(refusal(format!(
                        "delivery {seq} is not covered by its durable cursor tail"
                    )));
                }
            }
            let source_message = integer(row, "message_id")?;
            if agent.is_none() {
                // The archive cannot vouch for source-side visibility authority
                // that disappeared from a current shared-mailbox schema.
                let delivery = rows(
                    source,
                    "SELECT project_id FROM project_mailbox_deliveries WHERE message_id = ?",
                    &[Value::BigInt(source_message)],
                )?;
                if delivery.len() != 1 || integer(&delivery[0], "project_id")? != project {
                    return Err(refusal(format!(
                        "shared delivery {seq} has no source visibility authority"
                    )));
                }
            }
            let (project, agent, message) =
                mapped_delivery(target, identities, project, agent, source_message)?;
            if agent.is_none() {
                let delivery = rows(
                    target,
                    "SELECT project_id FROM project_mailbox_deliveries WHERE message_id = ?",
                    &[Value::BigInt(message)],
                )?;
                if delivery.len() != 1 || integer(&delivery[0], "project_id")? != project {
                    return Err(refusal(format!(
                        "shared delivery {seq} has no recovered visibility authority"
                    )));
                }
            }
            execute(
                target,
                "INSERT INTO inbox_delivery_events(seq, project_id, agent_id, message_id, kind, delivered_ts) VALUES (?, ?, ?, ?, ?, ?)",
                &[
                    Value::BigInt(seq),
                    Value::BigInt(project),
                    agent.map_or(Value::Null, Value::BigInt),
                    Value::BigInt(message),
                    Value::Text(kind),
                    Value::BigInt(integer(row, "delivered_ts")?),
                ],
            )?;
            remember_source_delivery(target, project, agent, message, source_message)?;
        }
        if page.len() < BATCH_ROWS {
            return Ok(());
        }
    }
}

fn restore_cursor_state(
    target: &CanonicalDbConn,
    source: &CanonicalDbConn,
    identities: &IdentityMaps<'_>,
    has_state: bool,
    ceiling: i64,
) -> DbResult<()> {
    if !has_state {
        // Match v34's legacy migration: retain the issued global ceiling for
        // existing scopes without inventing historical loss observations.
        for project in identities.projects.values() {
            write_state(target, *project, "project", *project, ceiling, 0)?;
        }
        for agent in identities.agents.values() {
            let owner = rows(
                target,
                "SELECT project_id FROM agents WHERE id = ?",
                &[Value::BigInt(*agent)],
            )?;
            let project = owner
                .first()
                .ok_or_else(|| refusal("legacy cursor agent is missing"))?;
            write_state(
                target,
                integer(project, "project_id")?,
                "agent",
                *agent,
                ceiling,
                0,
            )?;
        }
        return Ok(());
    }
    let mut after = None;
    let mut seen = HashSet::new();
    loop {
        let page = batch(
            source,
            "SELECT rowid AS row_id, project_id, scope, scope_id, tail_seq, pruned_through_seq FROM inbox_delivery_cursor_state",
            "rowid",
            after,
        )?;
        for row in &page {
            after = Some(integer(row, "row_id")?);
            let source_project = integer(row, "project_id")?;
            let project = mapped(identities.projects, source_project, "cursor project")?;
            let scope = text(row, "scope")?;
            let source_scope = integer(row, "scope_id")?;
            let scope_id = match scope.as_str() {
                "project" if source_scope == source_project => project,
                "agent" => {
                    let agent = mapped(identities.agents, source_scope, "cursor agent")?;
                    let owner = rows(
                        target,
                        "SELECT project_id FROM agents WHERE id = ?",
                        &[Value::BigInt(agent)],
                    )?;
                    if owner.len() != 1 || integer(&owner[0], "project_id")? != project {
                        return Err(refusal("cursor agent maps outside its project"));
                    }
                    agent
                }
                _ => return Err(refusal("invalid durable cursor scope")),
            };
            if !seen.insert((project, scope.clone(), scope_id)) {
                return Err(refusal(
                    "distinct durable cursor scopes collapse onto one recovered identity",
                ));
            }
            let tail = integer(row, "tail_seq")?;
            let pruned = integer(row, "pruned_through_seq")?;
            if tail < 0 || pruned < 0 || pruned > tail {
                return Err(refusal("invalid durable cursor tail/loss bounds"));
            }
            write_state(target, project, &scope, scope_id, tail, pruned)?;
        }
        if page.len() < BATCH_ROWS {
            return Ok(());
        }
    }
}

fn write_state(
    target: &CanonicalDbConn,
    project: i64,
    scope: &str,
    scope_id: i64,
    tail: i64,
    pruned: i64,
) -> DbResult<()> {
    execute(
        target,
        "INSERT INTO inbox_delivery_cursor_state(project_id, scope, scope_id, tail_seq, pruned_through_seq) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(project_id, scope, scope_id) DO UPDATE SET \
             tail_seq = MAX(inbox_delivery_cursor_state.tail_seq, excluded.tail_seq), \
             pruned_through_seq = MAX(inbox_delivery_cursor_state.pruned_through_seq, excluded.pruned_through_seq)",
        &[
            Value::BigInt(project),
            Value::Text(scope.to_string()),
            Value::BigInt(scope_id),
            Value::BigInt(tail),
            Value::BigInt(pruned),
        ],
    )
}
