//! Schema-level mailbox invariants for relational drift detection.
//!
//! These checks intentionally use plain SQL over the real mailbox database so
//! doctor/reconstruct paths and fuzz/regression tests can share one source of
//! truth instead of copying fragile ad hoc queries.

use std::collections::BTreeMap;

use asupersync::{Cx, Outcome};
use serde::{Deserialize, Serialize};

use crate::{DbConn, DbError, DbPool, DbResult};

/// Replay command emitted by invariant fuzz/regression harnesses.
pub const SCHEMA_INVARIANT_REPLAY_COMMAND: &str =
    "rch exec -- cargo test -p mcp-agent-mail-db schema_invariants -- --nocapture";

/// Logical surfaces covered by the SQLite schema invariant checker.
pub const SCHEMA_INVARIANT_SCOPES: &[&str] = &[
    "projects",
    "agents",
    "messages",
    "message_recipients",
    "inbox_stats",
    "file_reservations",
    "file_reservation_releases",
    "agent_links",
    "products",
    "product_project_links",
    "threads_via_messages.thread_id",
    "build_slots:file_backed_external",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaInvariantKind {
    OrphanAgentProject,
    OrphanInboxStatsAgent,
    OrphanMessageProject,
    OrphanMessageSender,
    OrphanMessageRecipientMessage,
    OrphanMessageRecipientAgent,
    CrossProjectMessageRecipient,
    AckBeforeRead,
    OrphanFileReservationProject,
    OrphanFileReservationAgent,
    FileReservationExpiryBeforeCreate,
    OrphanFileReservationRelease,
    FileReservationReleaseBeforeCreate,
    OrphanAgentLinkSource,
    OrphanAgentLinkTarget,
    InvalidAgentLinkStatus,
    AgentLinkExpiryBeforeCreate,
    OrphanProductProjectProduct,
    OrphanProductProjectProject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaInvariantFinding {
    pub kind: SchemaInvariantKind,
    pub table: String,
    pub count: i64,
    pub detail: String,
    pub replay_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaInvariantReport {
    pub checked_scopes: Vec<String>,
    pub table_counts: BTreeMap<String, i64>,
    pub findings: Vec<SchemaInvariantFinding>,
}

impl SchemaInvariantReport {
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.findings.is_empty()
    }

    #[must_use]
    pub fn finding_count(&self, kind: SchemaInvariantKind) -> i64 {
        self.findings
            .iter()
            .filter(|finding| finding.kind == kind)
            .map(|finding| finding.count)
            .sum()
    }
}

struct CountCheck {
    kind: SchemaInvariantKind,
    table: &'static str,
    detail: &'static str,
    sql: &'static str,
}

const TABLE_COUNT_SQL: &[(&str, &str)] = &[
    ("projects", "SELECT COUNT(*) AS count FROM projects"),
    ("agents", "SELECT COUNT(*) AS count FROM agents"),
    ("messages", "SELECT COUNT(*) AS count FROM messages"),
    (
        "message_recipients",
        "SELECT COUNT(*) AS count FROM message_recipients",
    ),
    ("inbox_stats", "SELECT COUNT(*) AS count FROM inbox_stats"),
    (
        "file_reservations",
        "SELECT COUNT(*) AS count FROM file_reservations",
    ),
    (
        "file_reservation_releases",
        "SELECT COUNT(*) AS count FROM file_reservation_releases",
    ),
    ("agent_links", "SELECT COUNT(*) AS count FROM agent_links"),
    ("products", "SELECT COUNT(*) AS count FROM products"),
    (
        "product_project_links",
        "SELECT COUNT(*) AS count FROM product_project_links",
    ),
];

const INVARIANT_CHECKS: &[CountCheck] = &[
    CountCheck {
        kind: SchemaInvariantKind::OrphanAgentProject,
        table: "agents",
        detail: "agent.project_id must reference projects.id",
        sql: "SELECT COUNT(*) AS count \
              FROM agents a LEFT JOIN projects p ON p.id = a.project_id \
              WHERE p.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanInboxStatsAgent,
        table: "inbox_stats",
        detail: "inbox_stats.agent_id must reference agents.id",
        sql: "SELECT COUNT(*) AS count \
              FROM inbox_stats s LEFT JOIN agents a ON a.id = s.agent_id \
              WHERE a.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanMessageProject,
        table: "messages",
        detail: "messages.project_id must reference projects.id",
        sql: "SELECT COUNT(*) AS count \
              FROM messages m LEFT JOIN projects p ON p.id = m.project_id \
              WHERE p.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanMessageSender,
        table: "messages",
        detail: "messages.sender_id must reference an agent in the same project",
        sql: "SELECT COUNT(*) AS count \
              FROM messages m \
              LEFT JOIN agents a ON a.id = m.sender_id AND a.project_id = m.project_id \
              WHERE a.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanMessageRecipientMessage,
        table: "message_recipients",
        detail: "message_recipients.message_id must reference messages.id",
        sql: "SELECT COUNT(*) AS count \
              FROM message_recipients r LEFT JOIN messages m ON m.id = r.message_id \
              WHERE m.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanMessageRecipientAgent,
        table: "message_recipients",
        detail: "message_recipients.agent_id must reference agents.id",
        sql: "SELECT COUNT(*) AS count \
              FROM message_recipients r LEFT JOIN agents a ON a.id = r.agent_id \
              WHERE a.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::CrossProjectMessageRecipient,
        table: "message_recipients",
        detail: "message recipients must belong to the same project as their message",
        sql: "SELECT COUNT(*) AS count \
              FROM message_recipients r \
              JOIN messages m ON m.id = r.message_id \
              JOIN agents a ON a.id = r.agent_id \
              WHERE a.project_id != m.project_id",
    },
    CountCheck {
        kind: SchemaInvariantKind::AckBeforeRead,
        table: "message_recipients",
        detail: "ack_ts must not exist before read_ts",
        sql: "SELECT COUNT(*) AS count \
              FROM message_recipients \
              WHERE ack_ts IS NOT NULL AND (read_ts IS NULL OR ack_ts < read_ts)",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanFileReservationProject,
        table: "file_reservations",
        detail: "file_reservations.project_id must reference projects.id",
        sql: "SELECT COUNT(*) AS count \
              FROM file_reservations f LEFT JOIN projects p ON p.id = f.project_id \
              WHERE p.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanFileReservationAgent,
        table: "file_reservations",
        detail: "file_reservations.agent_id must reference an agent in the same project",
        sql: "SELECT COUNT(*) AS count \
              FROM file_reservations f \
              LEFT JOIN agents a ON a.id = f.agent_id AND a.project_id = f.project_id \
              WHERE a.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::FileReservationExpiryBeforeCreate,
        table: "file_reservations",
        detail: "file reservation expires_ts must be greater than or equal to created_ts",
        sql: "SELECT COUNT(*) AS count \
              FROM file_reservations \
              WHERE expires_ts < created_ts",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanFileReservationRelease,
        table: "file_reservation_releases",
        detail: "file_reservation_releases.reservation_id must reference file_reservations.id",
        sql: "SELECT COUNT(*) AS count \
              FROM file_reservation_releases r \
              LEFT JOIN file_reservations f ON f.id = r.reservation_id \
              WHERE f.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::FileReservationReleaseBeforeCreate,
        table: "file_reservation_releases",
        detail: "file reservation release timestamp must not precede reservation creation",
        sql: "SELECT COUNT(*) AS count \
              FROM file_reservation_releases r \
              JOIN file_reservations f ON f.id = r.reservation_id \
              WHERE r.released_ts < f.created_ts",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanAgentLinkSource,
        table: "agent_links",
        detail: "agent_links source project/agent pair must exist",
        sql: "SELECT COUNT(*) AS count \
              FROM agent_links l \
              LEFT JOIN agents a ON a.id = l.a_agent_id AND a.project_id = l.a_project_id \
              WHERE a.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanAgentLinkTarget,
        table: "agent_links",
        detail: "agent_links target project/agent pair must exist",
        sql: "SELECT COUNT(*) AS count \
              FROM agent_links l \
              LEFT JOIN agents a ON a.id = l.b_agent_id AND a.project_id = l.b_project_id \
              WHERE a.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::InvalidAgentLinkStatus,
        table: "agent_links",
        detail: "agent_links.status must be pending, approved, or blocked",
        sql: "SELECT COUNT(*) AS count \
              FROM agent_links \
              WHERE status NOT IN ('pending', 'approved', 'blocked')",
    },
    CountCheck {
        kind: SchemaInvariantKind::AgentLinkExpiryBeforeCreate,
        table: "agent_links",
        detail: "agent_links.expires_ts must not precede created_ts",
        sql: "SELECT COUNT(*) AS count \
              FROM agent_links \
              WHERE expires_ts IS NOT NULL AND expires_ts < created_ts",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanProductProjectProduct,
        table: "product_project_links",
        detail: "product_project_links.product_id must reference products.id",
        sql: "SELECT COUNT(*) AS count \
              FROM product_project_links l LEFT JOIN products p ON p.id = l.product_id \
              WHERE p.id IS NULL",
    },
    CountCheck {
        kind: SchemaInvariantKind::OrphanProductProjectProject,
        table: "product_project_links",
        detail: "product_project_links.project_id must reference projects.id",
        sql: "SELECT COUNT(*) AS count \
              FROM product_project_links l LEFT JOIN projects p ON p.id = l.project_id \
              WHERE p.id IS NULL",
    },
];

fn count_query(conn: &DbConn, sql: &str, purpose: &str) -> DbResult<i64> {
    let rows = conn
        .query_sync(sql, &[])
        .map_err(|error| DbError::Sqlite(format!("{purpose}: {error}")))?;
    rows.first()
        .and_then(|row| row.get_named::<i64>("count").ok())
        .ok_or_else(|| DbError::Internal(format!("{purpose}: missing count column")))
}

/// Check schema invariants using an already-open connection.
pub fn check_schema_invariants_conn(conn: &DbConn) -> DbResult<SchemaInvariantReport> {
    let mut table_counts = BTreeMap::new();
    for (table, sql) in TABLE_COUNT_SQL {
        table_counts.insert((*table).to_string(), count_query(conn, sql, table)?);
    }

    let mut findings = Vec::new();
    for check in INVARIANT_CHECKS {
        let count = count_query(conn, check.sql, check.detail)?;
        if count > 0 {
            findings.push(SchemaInvariantFinding {
                kind: check.kind,
                table: check.table.to_string(),
                count,
                detail: check.detail.to_string(),
                replay_command: SCHEMA_INVARIANT_REPLAY_COMMAND.to_string(),
            });
        }
    }

    Ok(SchemaInvariantReport {
        checked_scopes: SCHEMA_INVARIANT_SCOPES
            .iter()
            .map(|scope| (*scope).to_string())
            .collect(),
        table_counts,
        findings,
    })
}

/// Check schema invariants through a pooled connection.
pub async fn check_schema_invariants(
    cx: &Cx,
    pool: &DbPool,
) -> Outcome<SchemaInvariantReport, DbError> {
    match pool.acquire(cx).await {
        Outcome::Ok(conn) => match check_schema_invariants_conn(&conn) {
            Ok(report) => Outcome::Ok(report),
            Err(error) => Outcome::Err(error),
        },
        Outcome::Err(error) => Outcome::Err(DbError::Sqlite(error.to_string())),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(panic) => Outcome::Panicked(panic),
    }
}
