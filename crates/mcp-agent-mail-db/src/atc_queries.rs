//! ATC policy engine read path — rate-limited queries for rollups and open
//! experiences.  Designed to avoid contending with the write hot path.
//!
//! Also provides the ack-overdue sweep: identifies open experiences whose
//! attribution window has elapsed and returns them as expiry candidates.

use std::collections::BTreeMap;
use std::time::Instant;

use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::atc_labeling::attribution_window;
use mcp_agent_mail_core::atc_retention::{LearningArtifactKind, retention_rule};
use mcp_agent_mail_core::experience::{EffectKind, ExperienceOutcome, ExperienceRow};
use sqlmodel_core::{Row, Value};

use crate::error::DbError;
use crate::pool::DbPool;

// ── Rollup read path ────────────────────────────────────────────────

/// Five-minute default rollup refresh window.
pub const DEFAULT_ROLLUP_REFRESH_LOOKBACK_MICROS: i64 = 300_000_000;

/// Hard cap on the number of strata a single refresh is allowed to touch.
///
/// The current ATC taxonomy is bounded by subsystem × effect_kind ×
/// risk_tier. Keeping the cap explicit prevents accidental cardinality
/// blowups from malformed effect metadata or future schema drift.
pub const MAX_ROLLUP_STRATA_PER_REFRESH: usize = 64;

const EWMA_LAMBDA: f64 = 0.95;

/// Typed rollup row for policy engine consumption.
#[derive(Debug, Clone, PartialEq)]
pub struct RollupEntry {
    pub stratum_key: String,
    pub subsystem: String,
    pub effect_kind: String,
    pub risk_tier: i32,
    pub total_count: i64,
    pub resolved_count: i64,
    pub censored_count: i64,
    pub expired_count: i64,
    pub correct_count: i64,
    pub incorrect_count: i64,
    pub total_regret: f64,
    pub total_loss: f64,
    pub ewma_loss: f64,
    pub ewma_weight: f64,
    pub delay_sum_micros: i64,
    pub delay_count: i64,
    pub delay_max_micros: i64,
    pub last_updated_ts: i64,
}

const ROLLUP_SELECT_SQL: &str = "\
    SELECT stratum_key, subsystem, effect_kind, risk_tier, \
           total_count, resolved_count, censored_count, expired_count, \
           correct_count, incorrect_count, total_regret, total_loss, \
           ewma_loss, ewma_weight, delay_sum_micros, delay_count, \
           delay_max_micros, last_updated_ts, \
           compacted_total_count, compacted_resolved_count, \
           compacted_censored_count, compacted_expired_count, \
           compacted_correct_count, compacted_incorrect_count, \
           compacted_total_regret, compacted_total_loss, \
           compacted_ewma_loss, compacted_ewma_weight, \
           compacted_delay_sum_micros, compacted_delay_count, \
           compacted_delay_max_micros, compacted_last_updated_ts \
    FROM atc_experience_rollups";

const TOUCHED_STRATA_SQL: &str = "\
    SELECT subsystem, effect_kind \
    FROM atc_experiences \
    WHERE created_ts >= ? OR COALESCE(resolved_ts, 0) >= ? \
    GROUP BY subsystem, effect_kind \
    ORDER BY subsystem, effect_kind";

const ROLLUP_REFRESH_SELECT_PREFIX_SQL: &str = "\
    SELECT experience_id, subsystem, effect_kind, state, outcome_json, \
           created_ts, resolved_ts \
    FROM atc_experiences WHERE ";

const RETENTION_COMPACT_SELECT_SQL: &str = "\
    SELECT experience_id, subsystem, effect_kind, state, outcome_json, \
           created_ts, resolved_ts \
    FROM atc_experiences \
    WHERE state IN ('resolved', 'censored', 'expired') \
      AND resolved_ts IS NOT NULL \
      AND resolved_ts <= ? \
    ORDER BY subsystem, effect_kind, COALESCE(resolved_ts, created_ts), experience_id";

const ROLLUP_REFRESH_UPSERT_SQL: &str = "\
    INSERT INTO atc_experience_rollups \
        (stratum_key, subsystem, effect_kind, risk_tier, \
         total_count, resolved_count, censored_count, expired_count, \
         correct_count, incorrect_count, total_regret, total_loss, \
         ewma_loss, ewma_weight, delay_sum_micros, delay_count, \
         delay_max_micros, last_updated_ts) \
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
    ON CONFLICT(stratum_key) DO UPDATE SET \
        subsystem = excluded.subsystem, \
        effect_kind = excluded.effect_kind, \
        risk_tier = excluded.risk_tier, \
        total_count = excluded.total_count, \
        resolved_count = excluded.resolved_count, \
        censored_count = excluded.censored_count, \
        expired_count = excluded.expired_count, \
        correct_count = excluded.correct_count, \
        incorrect_count = excluded.incorrect_count, \
        total_regret = excluded.total_regret, \
        total_loss = excluded.total_loss, \
        ewma_loss = excluded.ewma_loss, \
        ewma_weight = excluded.ewma_weight, \
        delay_sum_micros = excluded.delay_sum_micros, \
        delay_count = excluded.delay_count, \
        delay_max_micros = excluded.delay_max_micros, \
        last_updated_ts = excluded.last_updated_ts";

const ROLLUP_COMPACTED_UPSERT_SQL: &str = "\
    INSERT INTO atc_experience_rollups \
        (stratum_key, subsystem, effect_kind, risk_tier, \
         total_count, resolved_count, censored_count, expired_count, \
         correct_count, incorrect_count, total_regret, total_loss, \
         ewma_loss, ewma_weight, delay_sum_micros, delay_count, \
         delay_max_micros, last_updated_ts, \
         compacted_total_count, compacted_resolved_count, \
         compacted_censored_count, compacted_expired_count, \
         compacted_correct_count, compacted_incorrect_count, \
         compacted_total_regret, compacted_total_loss, \
         compacted_ewma_loss, compacted_ewma_weight, \
         compacted_delay_sum_micros, compacted_delay_count, \
         compacted_delay_max_micros, compacted_last_updated_ts) \
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
    ON CONFLICT(stratum_key) DO UPDATE SET \
        compacted_total_count = excluded.compacted_total_count, \
        compacted_resolved_count = excluded.compacted_resolved_count, \
        compacted_censored_count = excluded.compacted_censored_count, \
        compacted_expired_count = excluded.compacted_expired_count, \
        compacted_correct_count = excluded.compacted_correct_count, \
        compacted_incorrect_count = excluded.compacted_incorrect_count, \
        compacted_total_regret = excluded.compacted_total_regret, \
        compacted_total_loss = excluded.compacted_total_loss, \
        compacted_ewma_loss = excluded.compacted_ewma_loss, \
        compacted_ewma_weight = excluded.compacted_ewma_weight, \
        compacted_delay_sum_micros = excluded.compacted_delay_sum_micros, \
        compacted_delay_count = excluded.compacted_delay_count, \
        compacted_delay_max_micros = excluded.compacted_delay_max_micros, \
        compacted_last_updated_ts = excluded.compacted_last_updated_ts";

const COMPACTED_ROLLUP_OFFSET: usize = 18;

/// Summary of one rollup refresh pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RollupSummary {
    pub lookback_micros: i64,
    pub rows_scanned: usize,
    pub rows_applied: usize,
    pub strata_updated: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TouchedStratum {
    stratum_key: String,
    subsystem: String,
    effect_kind: String,
    risk_tier: i32,
}

#[derive(Debug, Clone, PartialEq)]
struct StoredRollupEntry {
    visible: RollupEntry,
    compacted: RollupEntry,
}

#[derive(Debug, Clone)]
struct ExperienceAggregateRow {
    subsystem: String,
    effect_kind: String,
    state: String,
    outcome_json: Option<String>,
    created_ts_micros: i64,
    resolved_ts_micros: Option<i64>,
}

trait RollupConn {
    fn rollup_query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, String>;
    fn rollup_execute_sync(&self, sql: &str, params: &[Value]) -> Result<(), String>;
}

impl RollupConn for crate::DbConn {
    fn rollup_query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, String> {
        self.query_sync(sql, params)
            .map_err(|error| error.to_string())
    }

    fn rollup_execute_sync(&self, sql: &str, params: &[Value]) -> Result<(), String> {
        self.execute_sync(sql, params)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

impl RollupConn for sqlmodel_pool::PooledConnection<crate::DbConn> {
    fn rollup_query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, String> {
        self.query_sync(sql, params)
            .map_err(|error| error.to_string())
    }

    fn rollup_execute_sync(&self, sql: &str, params: &[Value]) -> Result<(), String> {
        self.execute_sync(sql, params)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

impl RollupConn for crate::CanonicalDbConn {
    fn rollup_query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, String> {
        self.query_sync(sql, params)
            .map_err(|error| error.to_string())
    }

    fn rollup_execute_sync(&self, sql: &str, params: &[Value]) -> Result<(), String> {
        self.execute_sync(sql, params)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn row_text_idx(row: &Row, idx: usize) -> String {
    row.get(idx)
        .and_then(|v| match v {
            Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn row_i64_idx(row: &Row, idx: usize) -> i64 {
    row.get(idx)
        .and_then(|v| match v {
            Value::BigInt(n) => Some(*n),
            Value::Int(n) => Some(i64::from(*n)),
            _ => None,
        })
        .unwrap_or(0)
}

fn row_f64_idx(row: &Row, idx: usize) -> f64 {
    row.get(idx)
        .and_then(|v| match v {
            Value::Double(n) => Some(*n),
            Value::Float(n) => Some(f64::from(*n)),
            Value::BigInt(n) => Some(*n as f64),
            Value::Int(n) => Some(f64::from(*n)),
            _ => None,
        })
        .unwrap_or(0.0)
}

fn blank_rollup_entry(
    stratum_key: String,
    subsystem: String,
    effect_kind: String,
    risk_tier: i32,
) -> RollupEntry {
    RollupEntry {
        stratum_key,
        subsystem,
        effect_kind,
        risk_tier,
        total_count: 0,
        resolved_count: 0,
        censored_count: 0,
        expired_count: 0,
        correct_count: 0,
        incorrect_count: 0,
        total_regret: 0.0,
        total_loss: 0.0,
        ewma_loss: 0.0,
        ewma_weight: 0.0,
        delay_sum_micros: 0,
        delay_count: 0,
        delay_max_micros: 0,
        last_updated_ts: 0,
    }
}

fn decode_rollup_row(row: &Row) -> RollupEntry {
    #[allow(clippy::cast_possible_truncation)]
    RollupEntry {
        stratum_key: row_text_idx(row, 0),
        subsystem: row_text_idx(row, 1),
        effect_kind: row_text_idx(row, 2),
        risk_tier: row_i64_idx(row, 3) as i32,
        total_count: row_i64_idx(row, 4),
        resolved_count: row_i64_idx(row, 5),
        censored_count: row_i64_idx(row, 6),
        expired_count: row_i64_idx(row, 7),
        correct_count: row_i64_idx(row, 8),
        incorrect_count: row_i64_idx(row, 9),
        total_regret: row_f64_idx(row, 10),
        total_loss: row_f64_idx(row, 11),
        ewma_loss: row_f64_idx(row, 12),
        ewma_weight: row_f64_idx(row, 13),
        delay_sum_micros: row_i64_idx(row, 14),
        delay_count: row_i64_idx(row, 15),
        delay_max_micros: row_i64_idx(row, 16),
        last_updated_ts: row_i64_idx(row, 17),
    }
}

fn decode_compacted_rollup_row(row: &Row, visible: &RollupEntry) -> RollupEntry {
    blank_rollup_entry(
        visible.stratum_key.clone(),
        visible.subsystem.clone(),
        visible.effect_kind.clone(),
        visible.risk_tier,
    )
    .with_compacted_metrics(row)
}

trait RollupEntryCompactedExt {
    fn with_compacted_metrics(self, row: &Row) -> Self;
}

impl RollupEntryCompactedExt for RollupEntry {
    fn with_compacted_metrics(mut self, row: &Row) -> Self {
        self.total_count = row_i64_idx(row, COMPACTED_ROLLUP_OFFSET);
        self.resolved_count = row_i64_idx(row, COMPACTED_ROLLUP_OFFSET + 1);
        self.censored_count = row_i64_idx(row, COMPACTED_ROLLUP_OFFSET + 2);
        self.expired_count = row_i64_idx(row, COMPACTED_ROLLUP_OFFSET + 3);
        self.correct_count = row_i64_idx(row, COMPACTED_ROLLUP_OFFSET + 4);
        self.incorrect_count = row_i64_idx(row, COMPACTED_ROLLUP_OFFSET + 5);
        self.total_regret = row_f64_idx(row, COMPACTED_ROLLUP_OFFSET + 6);
        self.total_loss = row_f64_idx(row, COMPACTED_ROLLUP_OFFSET + 7);
        self.ewma_loss = row_f64_idx(row, COMPACTED_ROLLUP_OFFSET + 8);
        self.ewma_weight = row_f64_idx(row, COMPACTED_ROLLUP_OFFSET + 9);
        self.delay_sum_micros = row_i64_idx(row, COMPACTED_ROLLUP_OFFSET + 10);
        self.delay_count = row_i64_idx(row, COMPACTED_ROLLUP_OFFSET + 11);
        self.delay_max_micros = row_i64_idx(row, COMPACTED_ROLLUP_OFFSET + 12);
        self.last_updated_ts = row_i64_idx(row, COMPACTED_ROLLUP_OFFSET + 13);
        self
    }
}

fn decode_experience_aggregate_row(row: &Row) -> ExperienceAggregateRow {
    ExperienceAggregateRow {
        subsystem: row_text_idx(row, 1),
        effect_kind: row_text_idx(row, 2),
        state: row_text_idx(row, 3),
        outcome_json: row.get(4).and_then(|value| match value {
            Value::Text(raw) if !raw.is_empty() => Some(raw.clone()),
            _ => None,
        }),
        created_ts_micros: row_i64_idx(row, 5),
        resolved_ts_micros: opt_i64(row, 6),
    }
}

fn risk_tier_for_effect_kind(raw: &str) -> Option<i32> {
    match parse_effect_kind(raw)? {
        EffectKind::Advisory | EffectKind::Probe | EffectKind::NoAction => Some(0),
        EffectKind::RoutingSuggestion | EffectKind::Backpressure => Some(1),
        EffectKind::Release | EffectKind::ForceReservation => Some(2),
    }
}

fn stratum_for_row(subsystem: &str, effect_kind: &str) -> Option<TouchedStratum> {
    let risk_tier = risk_tier_for_effect_kind(effect_kind)?;
    Some(TouchedStratum {
        stratum_key: format!("{subsystem}:{effect_kind}:{risk_tier}"),
        subsystem: subsystem.to_string(),
        effect_kind: effect_kind.to_string(),
        risk_tier,
    })
}

fn query_touched_strata_from_columns(
    rows: &[Row],
    subsystem_idx: usize,
    effect_kind_idx: usize,
) -> BTreeMap<String, TouchedStratum> {
    let mut strata = BTreeMap::new();
    for row in rows {
        let subsystem = row_text_idx(row, subsystem_idx);
        let effect_kind = row_text_idx(row, effect_kind_idx);
        if let Some(stratum) = stratum_for_row(&subsystem, &effect_kind) {
            strata.insert(stratum.stratum_key.clone(), stratum);
        }
    }
    strata
}

fn query_touched_strata(rows: &[Row]) -> BTreeMap<String, TouchedStratum> {
    query_touched_strata_from_columns(rows, 0, 1)
}

fn query_touched_strata_from_experience_rows(rows: &[Row]) -> BTreeMap<String, TouchedStratum> {
    query_touched_strata_from_columns(rows, 1, 2)
}

fn build_rollup_refresh_scan_query(
    strata: impl Iterator<Item = TouchedStratum>,
) -> (String, Vec<Value>) {
    let mut sql = String::from(ROLLUP_REFRESH_SELECT_PREFIX_SQL);
    let mut params = Vec::new();
    let mut wrote_any = false;
    for stratum in strata {
        if wrote_any {
            sql.push_str(" OR ");
        }
        wrote_any = true;
        sql.push_str("(subsystem = ? AND effect_kind = ?)");
        params.push(Value::Text(stratum.subsystem));
        params.push(Value::Text(stratum.effect_kind));
    }
    sql.push_str(
        " ORDER BY subsystem, effect_kind, COALESCE(resolved_ts, created_ts), experience_id",
    );
    (sql, params)
}

fn build_existing_rollups_query(stratum_count: usize) -> String {
    let placeholders = std::iter::repeat_n("?", stratum_count)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{ROLLUP_SELECT_SQL} WHERE stratum_key IN ({placeholders}) ORDER BY stratum_key")
}

fn decode_existing_rollups(rows: &[Row]) -> BTreeMap<String, StoredRollupEntry> {
    rows.iter()
        .map(|row| {
            let visible = decode_rollup_row(row);
            let compacted = decode_compacted_rollup_row(row, &visible);
            (
                visible.stratum_key.clone(),
                StoredRollupEntry { visible, compacted },
            )
        })
        .collect()
}

fn tracked_rollup_state(state: &str) -> bool {
    matches!(state, "open" | "resolved" | "censored" | "expired")
}

fn apply_resolution_delay(
    entry: &mut RollupEntry,
    created_ts_micros: i64,
    resolved_ts_micros: Option<i64>,
) {
    let Some(resolved_ts_micros) = resolved_ts_micros else {
        return;
    };
    let delay_micros = resolved_ts_micros.saturating_sub(created_ts_micros);
    entry.delay_sum_micros = entry.delay_sum_micros.saturating_add(delay_micros);
    entry.delay_count = entry.delay_count.saturating_add(1);
    entry.delay_max_micros = entry.delay_max_micros.max(delay_micros);
}

fn apply_resolved_outcome(entry: &mut RollupEntry, outcome: Option<ExperienceOutcome>) {
    let Some(outcome) = outcome else {
        return;
    };
    if outcome.correct {
        entry.correct_count = entry.correct_count.saturating_add(1);
    } else {
        entry.incorrect_count = entry.incorrect_count.saturating_add(1);
    }
    let actual_loss = outcome.actual_loss.unwrap_or(0.0);
    let regret = outcome.regret.unwrap_or(0.0);
    entry.total_loss += actual_loss;
    entry.total_regret += regret;
    if entry.ewma_weight == 0.0 {
        entry.ewma_loss = actual_loss;
        entry.ewma_weight = 1.0;
        return;
    }
    let one_minus_lambda = 1.0 - EWMA_LAMBDA;
    entry.ewma_loss = EWMA_LAMBDA.mul_add(entry.ewma_loss, one_minus_lambda * actual_loss);
    entry.ewma_weight = EWMA_LAMBDA.mul_add(entry.ewma_weight, 1.0);
}

fn apply_rollup_row(entry: &mut RollupEntry, row: &ExperienceAggregateRow) {
    entry.total_count = entry.total_count.saturating_add(1);
    entry.last_updated_ts = entry
        .last_updated_ts
        .max(row.resolved_ts_micros.unwrap_or(row.created_ts_micros));

    match row.state.as_str() {
        "open" => {}
        "resolved" => {
            entry.resolved_count = entry.resolved_count.saturating_add(1);
            let outcome = row
                .outcome_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<ExperienceOutcome>(raw).ok());
            apply_resolved_outcome(entry, outcome);
            apply_resolution_delay(entry, row.created_ts_micros, row.resolved_ts_micros);
        }
        "censored" => {
            entry.censored_count = entry.censored_count.saturating_add(1);
            apply_resolution_delay(entry, row.created_ts_micros, row.resolved_ts_micros);
        }
        "expired" => {
            entry.expired_count = entry.expired_count.saturating_add(1);
            apply_resolution_delay(entry, row.created_ts_micros, row.resolved_ts_micros);
        }
        _ => {}
    }
}

fn accumulate_rollup_row(
    aggregates: &mut BTreeMap<String, RollupEntry>,
    row: ExperienceAggregateRow,
) {
    if !tracked_rollup_state(&row.state) {
        return;
    }
    let Some(stratum) = stratum_for_row(&row.subsystem, &row.effect_kind) else {
        return;
    };
    let entry = aggregates
        .entry(stratum.stratum_key.clone())
        .or_insert_with(|| {
            blank_rollup_entry(
                stratum.stratum_key,
                stratum.subsystem,
                stratum.effect_kind,
                stratum.risk_tier,
            )
        });
    apply_rollup_row(entry, &row);
}

fn finalize_rollups(rows: &[Row]) -> BTreeMap<String, RollupEntry> {
    finalize_rollups_with_seed(rows, &BTreeMap::new())
}

fn finalize_rollups_with_seed(
    rows: &[Row],
    seed: &BTreeMap<String, RollupEntry>,
) -> BTreeMap<String, RollupEntry> {
    let mut aggregates = seed.clone();
    for row in rows {
        accumulate_rollup_row(&mut aggregates, decode_experience_aggregate_row(row));
    }
    aggregates
}

fn rollup_params(entry: &RollupEntry) -> Vec<Value> {
    vec![
        Value::Text(entry.stratum_key.clone()),
        Value::Text(entry.subsystem.clone()),
        Value::Text(entry.effect_kind.clone()),
        Value::Int(entry.risk_tier),
        Value::BigInt(entry.total_count),
        Value::BigInt(entry.resolved_count),
        Value::BigInt(entry.censored_count),
        Value::BigInt(entry.expired_count),
        Value::BigInt(entry.correct_count),
        Value::BigInt(entry.incorrect_count),
        Value::Double(entry.total_regret),
        Value::Double(entry.total_loss),
        Value::Double(entry.ewma_loss),
        Value::Double(entry.ewma_weight),
        Value::BigInt(entry.delay_sum_micros),
        Value::BigInt(entry.delay_count),
        Value::BigInt(entry.delay_max_micros),
        Value::BigInt(entry.last_updated_ts),
    ]
}

fn compacted_rollup_params(entry: &RollupEntry) -> Vec<Value> {
    vec![
        Value::BigInt(entry.total_count),
        Value::BigInt(entry.resolved_count),
        Value::BigInt(entry.censored_count),
        Value::BigInt(entry.expired_count),
        Value::BigInt(entry.correct_count),
        Value::BigInt(entry.incorrect_count),
        Value::Double(entry.total_regret),
        Value::Double(entry.total_loss),
        Value::Double(entry.ewma_loss),
        Value::Double(entry.ewma_weight),
        Value::BigInt(entry.delay_sum_micros),
        Value::BigInt(entry.delay_count),
        Value::BigInt(entry.delay_max_micros),
        Value::BigInt(entry.last_updated_ts),
    ]
}

fn upsert_rollup_entry(conn: &impl RollupConn, entry: &RollupEntry) -> Result<(), DbError> {
    let params = rollup_params(entry);
    conn.rollup_execute_sync(ROLLUP_REFRESH_UPSERT_SQL, &params)
        .map_err(|error| DbError::Sqlite(format!("refresh_rollups upsert: {error}")))
}

fn upsert_compacted_rollup_entry(
    conn: &impl RollupConn,
    entry: &RollupEntry,
) -> Result<(), DbError> {
    let mut params = rollup_params(entry);
    params.extend(compacted_rollup_params(entry));
    conn.rollup_execute_sync(ROLLUP_COMPACTED_UPSERT_SQL, &params)
        .map_err(|error| DbError::Sqlite(format!("retention_compact baseline upsert: {error}")))
}

fn refresh_rollups_with_conn(
    conn: &impl RollupConn,
    now_micros: i64,
    lookback_micros: i64,
) -> Result<RollupSummary, DbError> {
    let refresh_started = Instant::now();
    let cutoff_micros = now_micros.saturating_sub(lookback_micros.max(0));
    let touched_rows = conn
        .rollup_query_sync(
            TOUCHED_STRATA_SQL,
            &[Value::BigInt(cutoff_micros), Value::BigInt(cutoff_micros)],
        )
        .map_err(|error| DbError::Sqlite(format!("refresh_rollups touched strata: {error}")))?;
    let mut touched = query_touched_strata(&touched_rows);
    let touched_count_before = touched.len();

    tracing::debug!(
        event = "atc.rollup.refresh_start",
        lookback_micros,
        strata_count_before = touched_count_before,
        "starting ATC rollup refresh"
    );

    if touched_count_before > MAX_ROLLUP_STRATA_PER_REFRESH {
        tracing::warn!(
            event = "atc.rollup.cardinality_warning",
            stratum_type = "subsystem+effect_kind+risk_tier",
            current_count = touched_count_before,
            limit = MAX_ROLLUP_STRATA_PER_REFRESH,
            "ATC rollup refresh capped touched strata to prevent cardinality blowup"
        );
        touched = touched
            .into_iter()
            .take(MAX_ROLLUP_STRATA_PER_REFRESH)
            .collect();
    }

    if touched.is_empty() {
        let duration_micros =
            i64::try_from(refresh_started.elapsed().as_micros()).unwrap_or(i64::MAX);
        mcp_agent_mail_core::global_metrics()
            .atc
            .record_rollup_refresh(u64::try_from(duration_micros.max(0)).unwrap_or(u64::MAX));
        tracing::debug!(
            event = "atc.rollup.refresh_complete",
            duration_micros,
            strata_updated = 0,
            rows_scanned = 0,
            rows_applied = 0,
            "ATC rollup refresh found no touched strata"
        );
        return Ok(RollupSummary {
            lookback_micros,
            rows_scanned: 0,
            rows_applied: 0,
            strata_updated: 0,
        });
    }

    let existing_sql = build_existing_rollups_query(touched.len());
    let existing_params = touched.keys().cloned().map(Value::Text).collect::<Vec<_>>();
    let existing_rows = conn
        .rollup_query_sync(&existing_sql, &existing_params)
        .map_err(|error| DbError::Sqlite(format!("refresh_rollups existing rows: {error}")))?;
    let existing = decode_existing_rollups(&existing_rows);

    let (refresh_sql, refresh_params) = build_rollup_refresh_scan_query(touched.values().cloned());
    let raw_rows = conn
        .rollup_query_sync(&refresh_sql, &refresh_params)
        .map_err(|error| DbError::Sqlite(format!("refresh_rollups scan: {error}")))?;
    let compacted_seed = existing
        .iter()
        .map(|(stratum_key, row)| (stratum_key.clone(), row.compacted.clone()))
        .collect::<BTreeMap<_, _>>();
    let refreshed = finalize_rollups_with_seed(&raw_rows, &compacted_seed);

    conn.rollup_execute_sync("BEGIN IMMEDIATE", &[])
        .map_err(|error| DbError::Sqlite(format!("refresh_rollups begin: {error}")))?;
    let mut rows_applied = 0_usize;
    let write_result = (|| -> Result<(), DbError> {
        for (stratum_key, entry) in &refreshed {
            upsert_rollup_entry(conn, entry)?;
            rows_applied = rows_applied.saturating_add(1);
            let previous = existing.get(stratum_key).map(|row| &row.visible);
            if previous != Some(entry) {
                tracing::debug!(
                    event = "atc.rollup.stratum_updated",
                    stratum_key,
                    from_total_count = previous.map_or(0, |row| row.total_count),
                    to_total_count = entry.total_count,
                    from_resolved_count = previous.map_or(0, |row| row.resolved_count),
                    to_resolved_count = entry.resolved_count,
                    from_censored_count = previous.map_or(0, |row| row.censored_count),
                    to_censored_count = entry.censored_count,
                    from_expired_count = previous.map_or(0, |row| row.expired_count),
                    to_expired_count = entry.expired_count,
                    "updated ATC rollup stratum"
                );
            }
        }
        conn.rollup_execute_sync("COMMIT", &[])
            .map_err(|error| DbError::Sqlite(format!("refresh_rollups commit: {error}")))
    })();
    if let Err(error) = write_result {
        let _ = conn.rollup_execute_sync("ROLLBACK", &[]);
        return Err(error);
    }

    let duration_micros = i64::try_from(refresh_started.elapsed().as_micros()).unwrap_or(i64::MAX);
    mcp_agent_mail_core::global_metrics()
        .atc
        .record_rollup_refresh(u64::try_from(duration_micros.max(0)).unwrap_or(u64::MAX));
    tracing::debug!(
        event = "atc.rollup.refresh_complete",
        duration_micros,
        strata_updated = refreshed.len(),
        rows_scanned = raw_rows.len(),
        rows_applied,
        "completed ATC rollup refresh"
    );

    Ok(RollupSummary {
        lookback_micros,
        rows_scanned: raw_rows.len(),
        rows_applied,
        strata_updated: refreshed.len(),
    })
}

/// Refresh ATC rollups for any strata touched inside the lookback window.
///
/// The refresh path scans recently touched experiences to find impacted
/// strata, recomputes each impacted stratum from surviving raw rows, and
/// layers that scan on top of the durable compacted-history baseline for the
/// same stratum. This keeps post-compaction rollups monotone even after the
/// hot path trims raw terminal rows.
pub async fn refresh_rollups(
    cx: &Cx,
    pool: &DbPool,
    now_micros: i64,
    lookback_micros: i64,
) -> Outcome<RollupSummary, DbError> {
    if pool.sqlite_path() != ":memory:" {
        return match crate::queries::open_canonical_atc_conn(pool, "refresh_rollups") {
            Ok(conn) => {
                let result = refresh_rollups_with_conn(&conn, now_micros, lookback_micros);
                crate::queries::close_canonical_db_conn(conn, "refresh_rollups connection");
                match result {
                    Ok(summary) => Outcome::Ok(summary),
                    Err(error) => Outcome::Err(error),
                }
            }
            Err(error) => Outcome::Err(error),
        };
    }
    let conn = match pool.acquire(cx).await {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => return Outcome::Err(DbError::Sqlite(error.to_string())),
        Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => return Outcome::Panicked(payload),
    };
    match refresh_rollups_with_conn(&conn, now_micros, lookback_micros) {
        Ok(summary) => Outcome::Ok(summary),
        Err(error) => Outcome::Err(error),
    }
}

fn query_rollups_canonical(pool: &DbPool) -> Result<Vec<RollupEntry>, DbError> {
    let conn = crate::queries::open_canonical_atc_conn(pool, "query_rollups")?;
    let result = (|| {
        let rows = crate::queries::canonical_query_atc_rows(
            &conn,
            ROLLUP_SELECT_SQL,
            &[],
            "query_rollups",
        )?;
        Ok(rows.iter().map(decode_rollup_row).collect())
    })();
    crate::queries::close_canonical_db_conn(conn, "query_rollups connection");
    result
}

/// Read all rollup rows for the policy engine tick.
///
/// Uses a dedicated canonical connection for file-backed DBs to avoid
/// contending with the write hot path.
pub async fn query_rollups(cx: &Cx, pool: &DbPool) -> Outcome<Vec<RollupEntry>, DbError> {
    if pool.sqlite_path() != ":memory:" {
        return match query_rollups_canonical(pool) {
            Ok(rows) => Outcome::Ok(rows),
            Err(error) => Outcome::Err(error),
        };
    }
    let conn = match pool.acquire(cx).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(DbError::Sqlite(e.to_string())),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    match conn.query_sync(ROLLUP_SELECT_SQL, &[]) {
        Ok(rows) => Outcome::Ok(rows.iter().map(decode_rollup_row).collect()),
        Err(error) => Outcome::Err(DbError::Sqlite(format!("query_rollups: {error}"))),
    }
}

// ── Open experience read path ───────────────────────────────────────

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Filter criteria for open experiences.
#[derive(Debug, Clone, Default)]
pub struct OpenExperienceFilter {
    /// Only return experiences for this subsystem.
    pub subsystem: Option<String>,
    /// Only return experiences for this subject (agent or thread).
    pub subject: Option<String>,
    /// Only return experiences for this project.
    pub project_key: Option<String>,
    /// Only return experiences created at or after this timestamp.
    pub since_ts_micros: Option<i64>,
    /// Only return experiences in this canonical stratum key.
    pub stratum_key: Option<String>,
    /// Maximum number of rows to return.
    pub limit: Option<u32>,
}

/// Inclusive sequence bounds for replaying ATC experience rows.
///
/// `experience_id` is the durable monotone sequence for replay/audit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SequenceRange {
    pub from_seq: Option<u64>,
    pub to_seq: Option<u64>,
}

impl SequenceRange {
    fn sql_bounds(self) -> Result<(Option<i64>, Option<i64>), DbError> {
        let from_seq = self
            .from_seq
            .map(|value| {
                i64::try_from(value).map_err(|_| {
                    DbError::invalid(
                        "from_seq",
                        format!("from_seq exceeds SQLite INTEGER range: {value}"),
                    )
                })
            })
            .transpose()?;
        let to_seq = self
            .to_seq
            .map(|value| {
                i64::try_from(value).map_err(|_| {
                    DbError::invalid(
                        "to_seq",
                        format!("to_seq exceeds SQLite INTEGER range: {value}"),
                    )
                })
            })
            .transpose()?;
        if let (Some(from_seq), Some(to_seq)) = (from_seq, to_seq)
            && from_seq > to_seq
        {
            return Err(DbError::invalid(
                "range",
                format!("from_seq {from_seq} must be <= to_seq {to_seq}"),
            ));
        }
        Ok((from_seq, to_seq))
    }
}

/// Stable replay payload for ATC raw experience rows.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperienceStream {
    pub range: SequenceRange,
    pub rows: Vec<ExperienceRow>,
}

/// Result of compacting aged ATC raw experience rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactSummary {
    pub max_age_micros: i64,
    pub cutoff_ts_micros: i64,
    pub deleted_rows: usize,
    pub preserved_rollups: bool,
}

/// Lightweight open-experience summary for the policy engine.
#[derive(Debug, Clone)]
pub struct OpenExperienceSummary {
    pub experience_id: i64,
    pub decision_id: i64,
    pub effect_id: i64,
    pub trace_id: String,
    pub state: String,
    pub subsystem: String,
    pub decision_class: String,
    pub subject: String,
    pub project_key: Option<String>,
    pub effect_kind: String,
    pub action: String,
    pub expected_loss: f64,
    pub created_ts_micros: i64,
    pub dispatched_ts_micros: Option<i64>,
    pub executed_ts_micros: Option<i64>,
}

impl From<&ExperienceRow> for OpenExperienceSummary {
    fn from(row: &ExperienceRow) -> Self {
        Self {
            experience_id: i64::try_from(row.experience_id).unwrap_or(i64::MAX),
            decision_id: i64::try_from(row.decision_id).unwrap_or(i64::MAX),
            effect_id: i64::try_from(row.effect_id).unwrap_or(i64::MAX),
            trace_id: row.trace_id.clone(),
            state: row.state.to_string(),
            subsystem: row.subsystem.to_string(),
            decision_class: row.decision_class.clone(),
            subject: row.subject.clone(),
            project_key: row.project_key.clone(),
            effect_kind: row.effect_kind.to_string(),
            action: row.action.clone(),
            expected_loss: row.expected_loss,
            created_ts_micros: row.created_ts_micros,
            dispatched_ts_micros: row.dispatched_ts_micros,
            executed_ts_micros: row.executed_ts_micros,
        }
    }
}

fn opt_i64(row: &Row, idx: usize) -> Option<i64> {
    row.get(idx).and_then(|v| match v {
        Value::BigInt(n) => Some(*n),
        Value::Int(n) => Some(i64::from(*n)),
        Value::Null => None,
        _ => None,
    })
}

#[cfg(test)]
fn decode_open_experience(row: &Row) -> OpenExperienceSummary {
    OpenExperienceSummary {
        experience_id: row_i64_idx(row, 0),
        decision_id: row_i64_idx(row, 1),
        effect_id: row_i64_idx(row, 2),
        trace_id: row_text_idx(row, 3),
        // skip claim_id (4), evidence_id (5)
        state: row_text_idx(row, 6),
        subsystem: row_text_idx(row, 7),
        decision_class: row_text_idx(row, 8),
        subject: row_text_idx(row, 9),
        project_key: row.get(10).and_then(|v| match v {
            Value::Text(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }),
        // skip policy_id (11)
        effect_kind: row_text_idx(row, 12),
        action: row_text_idx(row, 13),
        expected_loss: row_f64_idx(row, 14),
        created_ts_micros: row_i64_idx(row, 15),
        dispatched_ts_micros: opt_i64(row, 16),
        executed_ts_micros: opt_i64(row, 17),
    }
}

fn parse_open_experience_stratum(raw: &str) -> Result<TouchedStratum, DbError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(DbError::invalid(
            "stratum_key",
            "stratum_key must not be empty",
        ));
    }

    // Early fixtures used `|`; the current canonical form is `subsystem:effect:tier`.
    let delimiter = if trimmed.contains(':') {
        ':'
    } else if trimmed.contains('|') {
        '|'
    } else {
        return Err(DbError::invalid(
            "stratum_key",
            format!("invalid stratum_key '{trimmed}'; expected subsystem:effect_kind:risk_tier"),
        ));
    };
    let parts = trimmed.split(delimiter).collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(DbError::invalid(
            "stratum_key",
            format!("invalid stratum_key '{trimmed}'; expected subsystem:effect_kind:risk_tier"),
        ));
    }

    let subsystem = parts[0].trim();
    let effect_kind = parts[1].trim();
    let risk_tier = parts[2].trim().parse::<i32>().map_err(|error| {
        DbError::invalid(
            "stratum_key",
            format!("invalid risk tier in stratum_key '{trimmed}': {error}"),
        )
    })?;
    let canonical = stratum_for_row(subsystem, effect_kind).ok_or_else(|| {
        DbError::invalid(
            "stratum_key",
            format!("unknown ATC stratum effect_kind '{effect_kind}'"),
        )
    })?;
    if canonical.risk_tier != risk_tier {
        return Err(DbError::invalid(
            "stratum_key",
            format!(
                "risk tier {risk_tier} does not match canonical tier {} for {subsystem}:{effect_kind}",
                canonical.risk_tier
            ),
        ));
    }
    Ok(canonical)
}

fn build_open_experience_query(
    filter: &OpenExperienceFilter,
) -> Result<(String, Vec<Value>), DbError> {
    let mut sql = format!(
        "{} WHERE state IN ('open', 'dispatched', 'executed', 'planned')",
        crate::queries::ATC_EXPERIENCE_SELECT_COLUMNS_SQL
    );
    let mut params = Vec::new();
    if let Some(ref subsystem) = filter.subsystem {
        sql.push_str(" AND subsystem = ?");
        params.push(Value::Text(subsystem.clone()));
    }
    if let Some(ref subject) = filter.subject {
        sql.push_str(" AND subject = ?");
        params.push(Value::Text(subject.clone()));
    }
    if let Some(ref project_key) = filter.project_key {
        sql.push_str(" AND project_key = ?");
        params.push(Value::Text(project_key.clone()));
    }
    if let Some(since_ts_micros) = filter.since_ts_micros {
        sql.push_str(" AND created_ts >= ?");
        params.push(Value::BigInt(since_ts_micros));
    }
    if let Some(ref stratum_key) = filter.stratum_key {
        let stratum = parse_open_experience_stratum(stratum_key)?;
        sql.push_str(" AND subsystem = ? AND effect_kind = ?");
        params.push(Value::Text(stratum.subsystem));
        params.push(Value::Text(stratum.effect_kind));
    }
    sql.push_str(" ORDER BY created_ts DESC, experience_id DESC");
    if let Some(limit) = filter.limit {
        use std::fmt::Write;
        let _ = write!(sql, " LIMIT {limit}");
    }
    Ok((sql, params))
}

fn build_replay_query(range: SequenceRange) -> Result<(String, Vec<Value>), DbError> {
    let (from_seq, to_seq) = range.sql_bounds()?;
    let mut sql = String::from(crate::queries::ATC_EXPERIENCE_SELECT_COLUMNS_SQL);
    let mut params = Vec::new();
    match (from_seq, to_seq) {
        (Some(from), Some(to)) => {
            sql.push_str(" WHERE experience_id >= ? AND experience_id <= ?");
            params.push(Value::BigInt(from));
            params.push(Value::BigInt(to));
        }
        (Some(from), None) => {
            sql.push_str(" WHERE experience_id >= ?");
            params.push(Value::BigInt(from));
        }
        (None, Some(to)) => {
            sql.push_str(" WHERE experience_id <= ?");
            params.push(Value::BigInt(to));
        }
        (None, None) => {}
    }
    sql.push_str(" ORDER BY experience_id ASC");
    Ok((sql, params))
}

fn decode_experience_rows(rows: &[Row]) -> Result<Vec<ExperienceRow>, DbError> {
    let mut decoded = Vec::with_capacity(rows.len());
    for row in rows {
        decoded.push(crate::queries::decode_atc_experience_row(row)?);
    }
    Ok(decoded)
}

fn query_open_experiences_canonical(
    pool: &DbPool,
    filter: &OpenExperienceFilter,
) -> Result<Vec<ExperienceRow>, DbError> {
    let conn = crate::queries::open_canonical_atc_conn(pool, "query_open_experiences")?;
    let result = (|| {
        let (sql, params) = build_open_experience_query(filter)?;
        let rows = crate::queries::canonical_query_atc_rows(
            &conn,
            &sql,
            &params,
            "query_open_experiences",
        )?;
        decode_experience_rows(&rows)
    })();
    crate::queries::close_canonical_db_conn(conn, "query_open_experiences connection");
    result
}

/// Read open/non-terminal experiences matching optional filters.
///
/// This returns full durable rows because the outcome sweep, operator surfaces,
/// and transparency/debug paths all need the same underlying state machine
/// contract, not an ad hoc projection.
pub async fn query_open_experiences(
    cx: &Cx,
    pool: &DbPool,
    filter: OpenExperienceFilter,
) -> Outcome<Vec<ExperienceRow>, DbError> {
    if pool.sqlite_path() != ":memory:" {
        match crate::queries::ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        return match query_open_experiences_canonical(pool, &filter) {
            Ok(rows) => Outcome::Ok(rows),
            Err(error) => Outcome::Err(error),
        };
    }

    let conn = match pool.acquire(cx).await {
        Outcome::Ok(c) => c,
        Outcome::Err(error) => return Outcome::Err(DbError::Sqlite(error.to_string())),
        Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => return Outcome::Panicked(payload),
    };
    let (sql, params) = match build_open_experience_query(&filter) {
        Ok(query) => query,
        Err(error) => return Outcome::Err(error),
    };
    match conn.query_sync(&sql, &params) {
        Ok(rows) => match decode_experience_rows(&rows) {
            Ok(rows) => Outcome::Ok(rows),
            Err(error) => Outcome::Err(error),
        },
        Err(error) => Outcome::Err(DbError::Sqlite(format!("query_open_experiences: {error}"))),
    }
}

fn minimum_retention_compaction_age_micros() -> Result<i64, DbError> {
    let rule = retention_rule(LearningArtifactKind::ResolvedExperienceRows).ok_or_else(|| {
        DbError::Internal("missing ATC retention rule for resolved experience rows".to_string())
    })?;
    Ok(i64::from(rule.compact_after_days.unwrap_or(rule.hot_days)) * MICROS_PER_DAY)
}

fn validate_retention_compaction(
    max_age_micros: i64,
    preserve_rollups: bool,
) -> Result<(), DbError> {
    if max_age_micros <= 0 {
        return Err(DbError::invalid(
            "max_age_micros",
            "max_age_micros must be positive",
        ));
    }

    let minimum_age_micros = minimum_retention_compaction_age_micros()?;
    if max_age_micros < minimum_age_micros {
        return Err(DbError::invalid(
            "max_age_micros",
            format!(
                "max_age_micros {max_age_micros} is below ATC retention policy minimum {minimum_age_micros}"
            ),
        ));
    }

    if !preserve_rollups {
        let rollup_rule =
            retention_rule(LearningArtifactKind::ExperienceRollups).ok_or_else(|| {
                DbError::Internal("missing ATC retention rule for experience rollups".to_string())
            })?;
        return Err(DbError::invalid(
            "preserve_rollups",
            format!(
                "rollups must remain queryable on the {:?} warm path while raw rows compact away",
                rollup_rule.primary_plane
            ),
        ));
    }

    Ok(())
}

fn retention_compact_with_conn(
    conn: &impl RollupConn,
    cutoff_ts_micros: i64,
) -> Result<usize, DbError> {
    conn.rollup_execute_sync("BEGIN IMMEDIATE", &[])
        .map_err(|error| DbError::Sqlite(format!("retention_compact begin: {error}")))?;
    let result = (|| -> Result<usize, DbError> {
        let doomed_rows = conn
            .rollup_query_sync(
                RETENTION_COMPACT_SELECT_SQL,
                &[Value::BigInt(cutoff_ts_micros)],
            )
            .map_err(|error| DbError::Sqlite(format!("retention_compact select: {error}")))?;
        if doomed_rows.is_empty() {
            conn.rollup_execute_sync("COMMIT", &[])
                .map_err(|error| DbError::Sqlite(format!("retention_compact commit: {error}")))?;
            return Ok(0);
        }

        let touched = query_touched_strata_from_experience_rows(&doomed_rows);
        let existing = if touched.is_empty() {
            BTreeMap::new()
        } else {
            let existing_sql = build_existing_rollups_query(touched.len());
            let existing_params = touched.keys().cloned().map(Value::Text).collect::<Vec<_>>();
            let existing_rows = conn
                .rollup_query_sync(&existing_sql, &existing_params)
                .map_err(|error| {
                    DbError::Sqlite(format!("retention_compact existing rows: {error}"))
                })?;
            decode_existing_rollups(&existing_rows)
        };
        let compacted_seed = existing
            .iter()
            .map(|(stratum_key, row)| (stratum_key.clone(), row.compacted.clone()))
            .collect::<BTreeMap<_, _>>();
        let updated_compacted = finalize_rollups_with_seed(&doomed_rows, &compacted_seed);

        for entry in updated_compacted.values() {
            upsert_compacted_rollup_entry(conn, entry)?;
        }
        conn.rollup_execute_sync(
            "DELETE FROM atc_experiences \
             WHERE state IN ('resolved', 'censored', 'expired') \
               AND resolved_ts IS NOT NULL \
               AND resolved_ts <= ?",
            &[Value::BigInt(cutoff_ts_micros)],
        )
        .map_err(|error| DbError::Sqlite(format!("retention_compact delete: {error}")))?;
        conn.rollup_execute_sync("COMMIT", &[])
            .map_err(|error| DbError::Sqlite(format!("retention_compact commit: {error}")))?;
        Ok(doomed_rows.len())
    })();
    if let Err(error) = result {
        let _ = conn.rollup_execute_sync("ROLLBACK", &[]);
        return Err(error);
    }
    result
}

fn retention_compact_canonical(pool: &DbPool, cutoff_ts_micros: i64) -> Result<usize, DbError> {
    let conn = crate::queries::open_canonical_atc_conn(pool, "retention_compact")?;
    let result = retention_compact_with_conn(&conn, cutoff_ts_micros);
    crate::queries::close_canonical_db_conn(conn, "retention_compact connection");
    result
}

/// Delete aged resolved/censored/expired raw rows while preserving rollups.
///
/// The ATC retention contract keeps unresolved rows queryable and forces
/// rollups to outlive raw resolved rows. This API enforces that policy.
pub async fn retention_compact(
    cx: &Cx,
    pool: &DbPool,
    max_age_micros: i64,
    preserve_rollups: bool,
) -> Outcome<CompactSummary, DbError> {
    if let Err(error) = validate_retention_compaction(max_age_micros, preserve_rollups) {
        return Outcome::Err(error);
    }

    let cutoff_ts_micros = crate::now_micros().saturating_sub(max_age_micros);
    let deleted_rows = if pool.sqlite_path() == ":memory:" {
        let conn = match pool.acquire(cx).await {
            Outcome::Ok(c) => c,
            Outcome::Err(error) => return Outcome::Err(DbError::Sqlite(error.to_string())),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        };
        match retention_compact_with_conn(&conn, cutoff_ts_micros) {
            Ok(rows) => rows,
            Err(error) => return Outcome::Err(error),
        }
    } else {
        match crate::queries::ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        match retention_compact_canonical(pool, cutoff_ts_micros) {
            Ok(rows) => rows,
            Err(error) => return Outcome::Err(error),
        }
    };

    Outcome::Ok(CompactSummary {
        max_age_micros,
        cutoff_ts_micros,
        deleted_rows,
        preserved_rollups: preserve_rollups,
    })
}

fn replay_canonical(pool: &DbPool, range: SequenceRange) -> Result<ExperienceStream, DbError> {
    let conn = crate::queries::open_canonical_atc_conn(pool, "replay_atc_experiences")?;
    let result = (|| {
        let (sql, params) = build_replay_query(range)?;
        let rows = crate::queries::canonical_query_atc_rows(
            &conn,
            &sql,
            &params,
            "replay_atc_experiences",
        )?;
        Ok(ExperienceStream {
            range,
            rows: decode_experience_rows(&rows)?,
        })
    })();
    crate::queries::close_canonical_db_conn(conn, "replay_atc_experiences connection");
    result
}

/// Replay raw ATC experience rows in stable `experience_id` order.
pub async fn replay(
    cx: &Cx,
    pool: &DbPool,
    range: SequenceRange,
) -> Outcome<ExperienceStream, DbError> {
    if pool.sqlite_path() != ":memory:" {
        match crate::queries::ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        return match replay_canonical(pool, range) {
            Ok(stream) => Outcome::Ok(stream),
            Err(error) => Outcome::Err(error),
        };
    }

    let conn = match pool.acquire(cx).await {
        Outcome::Ok(c) => c,
        Outcome::Err(error) => return Outcome::Err(DbError::Sqlite(error.to_string())),
        Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => return Outcome::Panicked(payload),
    };
    let (sql, params) = match build_replay_query(range) {
        Ok(query) => query,
        Err(error) => return Outcome::Err(error),
    };
    match conn.query_sync(&sql, &params) {
        Ok(rows) => match decode_experience_rows(&rows) {
            Ok(rows) => Outcome::Ok(ExperienceStream { range, rows }),
            Err(error) => Outcome::Err(error),
        },
        Err(error) => Outcome::Err(DbError::Sqlite(format!("replay_atc_experiences: {error}"))),
    }
}

// ── Ack-overdue / attribution-window expiry sweep ──────────────────

/// An open experience whose attribution window has elapsed.
#[derive(Debug, Clone)]
pub struct ExpiredExperienceCandidate {
    pub experience_id: u64,
    pub effect_kind: String,
    pub subject: String,
    pub subsystem: String,
    pub created_ts_micros: i64,
    pub window_micros: i64,
}

fn parse_effect_kind(raw: &str) -> Option<EffectKind> {
    match raw {
        "advisory" => Some(EffectKind::Advisory),
        "probe" => Some(EffectKind::Probe),
        "release" => Some(EffectKind::Release),
        "force_reservation" => Some(EffectKind::ForceReservation),
        "routing_suggestion" => Some(EffectKind::RoutingSuggestion),
        "backpressure" => Some(EffectKind::Backpressure),
        "no_action" => Some(EffectKind::NoAction),
        _ => None,
    }
}

fn experience_resolution_anchor_micros(exp: &OpenExperienceSummary) -> i64 {
    exp.executed_ts_micros
        .or(exp.dispatched_ts_micros)
        .unwrap_or(exp.created_ts_micros)
}

/// Identify open experiences whose attribution window has elapsed.
///
/// Scans all non-terminal experiences and compares each row's execution
/// anchor (`executed_ts_micros`, then `dispatched_ts_micros`, then
/// `created_ts_micros`) plus
/// `attribution_window(effect_kind).max_window_micros` against `now_micros`.
/// Returns candidates suitable for
/// `resolve_experience(..., ResolutionKind::Expired { ts_micros })`.
#[must_use]
pub fn find_expired_experiences(
    open: &[OpenExperienceSummary],
    now_micros: i64,
) -> Vec<ExpiredExperienceCandidate> {
    let mut candidates = Vec::new();
    for exp in open {
        let kind = match parse_effect_kind(&exp.effect_kind) {
            Some(k) => k,
            None => continue,
        };
        let window = attribution_window(kind);
        let deadline =
            experience_resolution_anchor_micros(exp).saturating_add(window.max_window_micros);
        if now_micros >= deadline {
            #[allow(clippy::cast_sign_loss)]
            candidates.push(ExpiredExperienceCandidate {
                experience_id: exp.experience_id as u64,
                effect_kind: exp.effect_kind.clone(),
                subject: exp.subject.clone(),
                subsystem: exp.subsystem.clone(),
                created_ts_micros: exp.created_ts_micros,
                window_micros: window.max_window_micros,
            });
        }
    }
    candidates
}

/// Query open experiences and return those whose attribution window has
/// elapsed, ready for expiry resolution.
///
/// Combines `query_open_experiences` with `find_expired_experiences` into
/// a single async call for sweep-loop convenience.
pub async fn sweep_expired_experiences(
    cx: &Cx,
    pool: &DbPool,
    now_micros: i64,
) -> Outcome<Vec<ExpiredExperienceCandidate>, DbError> {
    let open = match query_open_experiences(cx, pool, OpenExperienceFilter::default()).await {
        Outcome::Ok(rows) => rows,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let open = open
        .iter()
        .map(OpenExperienceSummary::from)
        .collect::<Vec<_>>();
    Outcome::Ok(find_expired_experiences(&open, now_micros))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use proptest::prelude::*;
    use std::sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    };

    static TEST_POOL_ID: AtomicU64 = AtomicU64::new(1);
    static TEST_POOL_DIRS: OnceLock<Mutex<Vec<tempfile::TempDir>>> = OnceLock::new();

    fn make_row(names: Vec<&str>, values: Vec<Value>) -> Row {
        Row::new(names.into_iter().map(String::from).collect(), values)
    }

    #[derive(Debug, Clone)]
    struct TestExperienceSpec {
        experience_id: i64,
        subsystem: String,
        effect_kind: String,
        state: String,
        created_ts_micros: i64,
        resolved_ts_micros: Option<i64>,
        correct: Option<bool>,
        actual_loss: Option<f64>,
        regret: Option<f64>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ExpectedCounts {
        total_count: i64,
        resolved_count: i64,
        censored_count: i64,
        expired_count: i64,
        correct_count: i64,
        incorrect_count: i64,
    }

    fn test_pool() -> DbPool {
        let pool_id = TEST_POOL_ID.fetch_add(1, Ordering::Relaxed);
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join(format!("atc_rollup_test_{pool_id}.db"));
        let init_conn = crate::CanonicalDbConn::open_file(db_path.display().to_string())
            .expect("open canonical ATC test db");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply ATC test pragmas");
        let base_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&base_sql)
            .expect("apply ATC test base schema");
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build ATC query setup runtime");
        let cx = Cx::for_testing();
        runtime
            .block_on(crate::schema::migrate_to_latest_base(&cx, &init_conn))
            .into_result()
            .expect("migrate ATC test schema");
        runtime
            .block_on(crate::schema::migrate_runtime_canonical_followup(
                &cx, &init_conn,
            ))
            .into_result()
            .expect("apply ATC canonical follow-up schema");
        crate::queries::close_canonical_db_conn(init_conn, "ATC rollup test init connection");

        crate::create_pool(&crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        })
        .map(|pool| {
            TEST_POOL_DIRS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .expect("lock test tempdir registry")
                .push(dir);
            pool
        })
        .expect("create file-backed ATC test pool")
    }

    fn encode_outcome(spec: &TestExperienceSpec) -> Option<String> {
        if spec.state != "resolved" {
            return None;
        }
        let outcome = ExperienceOutcome {
            observed_ts_micros: spec
                .resolved_ts_micros
                .expect("resolved rows need resolved timestamp"),
            label: if spec.correct.unwrap_or(false) {
                "correct"
            } else {
                "incorrect"
            }
            .to_string(),
            correct: spec.correct.unwrap_or(false),
            actual_loss: spec.actual_loss,
            regret: spec.regret,
            evidence: None,
        };
        Some(serde_json::to_string(&outcome).expect("serialize outcome"))
    }

    fn insert_experience(pool: &DbPool, spec: &TestExperienceSpec) {
        let conn =
            crate::queries::open_canonical_atc_conn(pool, "insert_atc_rollup_test_experience")
                .expect("open canonical ATC test connection");
        let params = vec![
            Value::BigInt(spec.experience_id),
            Value::BigInt(spec.experience_id),
            Value::BigInt(spec.experience_id),
            Value::Text(format!("trc-{}", spec.experience_id)),
            Value::Text(format!("clm-{}", spec.experience_id)),
            Value::Text(format!("evi-{}", spec.experience_id)),
            Value::Text(spec.state.clone()),
            Value::Text(spec.subsystem.clone()),
            Value::Text("unit_test".to_string()),
            Value::Text(format!("agent-{}", spec.experience_id)),
            Value::Text(spec.effect_kind.clone()),
            Value::Text("test_action".to_string()),
            Value::Double(0.0),
            Value::Int(1),
            Value::BigInt(spec.created_ts_micros),
            spec.resolved_ts_micros.map_or(Value::Null, Value::BigInt),
            encode_outcome(spec).map_or(Value::Null, Value::Text),
        ];
        conn.execute_sync(
            "INSERT INTO atc_experiences \
                (experience_id, decision_id, effect_id, trace_id, claim_id, evidence_id, \
                 state, subsystem, decision_class, subject, effect_kind, action, \
                 expected_loss, feature_schema_version, created_ts, resolved_ts, outcome_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &params,
        )
        .expect("insert ATC experience");
        crate::queries::close_canonical_db_conn(conn, "insert ATC test experience");
    }

    fn run_refresh(pool: &DbPool, now_micros: i64, lookback_micros: i64) -> RollupSummary {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build refresh runtime");
        let cx = Cx::for_testing();
        match runtime.block_on(refresh_rollups(&cx, pool, now_micros, lookback_micros)) {
            Outcome::Ok(summary) => summary,
            Outcome::Err(error) => panic!("refresh_rollups failed: {error}"),
            Outcome::Cancelled(reason) => panic!("refresh_rollups cancelled: {reason:?}"),
            Outcome::Panicked(payload) => std::panic::resume_unwind(Box::new(payload)),
        }
    }

    fn fetch_rollups(pool: &DbPool) -> Vec<RollupEntry> {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build query runtime");
        let cx = Cx::for_testing();
        match runtime.block_on(query_rollups(&cx, pool)) {
            Outcome::Ok(rows) => rows,
            Outcome::Err(error) => panic!("query_rollups failed: {error}"),
            Outcome::Cancelled(reason) => panic!("query_rollups cancelled: {reason:?}"),
            Outcome::Panicked(payload) => std::panic::resume_unwind(Box::new(payload)),
        }
    }

    fn query_open_rows(pool: &DbPool, filter: OpenExperienceFilter) -> Vec<ExperienceRow> {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build open-experience query runtime");
        let cx = Cx::for_testing();
        match runtime.block_on(query_open_experiences(&cx, pool, filter)) {
            Outcome::Ok(rows) => rows,
            Outcome::Err(error) => panic!("query_open_experiences failed: {error}"),
            Outcome::Cancelled(reason) => {
                panic!("query_open_experiences cancelled: {reason:?}");
            }
            Outcome::Panicked(payload) => std::panic::resume_unwind(Box::new(payload)),
        }
    }

    fn run_retention_compact(
        pool: &DbPool,
        max_age_micros: i64,
        preserve_rollups: bool,
    ) -> CompactSummary {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build compaction runtime");
        let cx = Cx::for_testing();
        match runtime.block_on(retention_compact(
            &cx,
            pool,
            max_age_micros,
            preserve_rollups,
        )) {
            Outcome::Ok(summary) => summary,
            Outcome::Err(error) => panic!("retention_compact failed: {error}"),
            Outcome::Cancelled(reason) => panic!("retention_compact cancelled: {reason:?}"),
            Outcome::Panicked(payload) => std::panic::resume_unwind(Box::new(payload)),
        }
    }

    fn run_replay(pool: &DbPool, range: SequenceRange) -> ExperienceStream {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build replay runtime");
        let cx = Cx::for_testing();
        match runtime.block_on(replay(&cx, pool, range)) {
            Outcome::Ok(stream) => stream,
            Outcome::Err(error) => panic!("replay failed: {error}"),
            Outcome::Cancelled(reason) => panic!("replay cancelled: {reason:?}"),
            Outcome::Panicked(payload) => std::panic::resume_unwind(Box::new(payload)),
        }
    }

    fn rollups_by_key(rows: Vec<RollupEntry>) -> BTreeMap<String, RollupEntry> {
        rows.into_iter()
            .map(|entry| (entry.stratum_key.clone(), entry))
            .collect()
    }

    fn expected_counts(specs: &[TestExperienceSpec]) -> BTreeMap<String, ExpectedCounts> {
        let mut counts = BTreeMap::new();
        for spec in specs {
            if !tracked_rollup_state(&spec.state) {
                continue;
            }
            let Some(stratum) = stratum_for_row(&spec.subsystem, &spec.effect_kind) else {
                continue;
            };
            let entry = counts.entry(stratum.stratum_key).or_insert(ExpectedCounts {
                total_count: 0,
                resolved_count: 0,
                censored_count: 0,
                expired_count: 0,
                correct_count: 0,
                incorrect_count: 0,
            });
            entry.total_count += 1;
            match spec.state.as_str() {
                "resolved" => {
                    entry.resolved_count += 1;
                    if spec.correct.unwrap_or(false) {
                        entry.correct_count += 1;
                    } else {
                        entry.incorrect_count += 1;
                    }
                }
                "censored" => entry.censored_count += 1,
                "expired" => entry.expired_count += 1,
                "open" => {}
                other => panic!("unexpected test state: {other}"),
            }
        }
        counts
    }

    fn max_touch_ts(specs: &[TestExperienceSpec]) -> i64 {
        specs
            .iter()
            .map(|spec| spec.resolved_ts_micros.unwrap_or(spec.created_ts_micros))
            .max()
            .unwrap_or(0)
    }

    fn effect_kind_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("probe".to_string()),
            Just("advisory".to_string()),
            Just("release".to_string()),
            Just("backpressure".to_string()),
        ]
    }

    fn state_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("open".to_string()),
            Just("resolved".to_string()),
            Just("censored".to_string()),
            Just("expired".to_string()),
        ]
    }

    fn experience_specs_strategy() -> impl Strategy<Value = Vec<TestExperienceSpec>> {
        prop::collection::vec((effect_kind_strategy(), state_strategy(), 0_u8..=1), 1..12).prop_map(
            |items| {
                items
                    .into_iter()
                    .enumerate()
                    .map(|(idx, (effect_kind, state, flag))| {
                        let created_ts_micros = 1_000_000 + (idx as i64 * 10_000);
                        let resolved_ts_micros = match state.as_str() {
                            "open" => None,
                            _ => Some(created_ts_micros + 5_000),
                        };
                        let resolved = state == "resolved";
                        TestExperienceSpec {
                            experience_id: (idx as i64) + 1,
                            subsystem: if idx % 2 == 0 {
                                "liveness".to_string()
                            } else {
                                "deadlock".to_string()
                            },
                            effect_kind,
                            state,
                            created_ts_micros,
                            resolved_ts_micros,
                            correct: resolved.then_some(flag == 1),
                            actual_loss: resolved.then_some(if flag == 1 { 0.0 } else { 1.5 }),
                            regret: resolved.then_some(if flag == 1 { 0.0 } else { 0.5 }),
                        }
                    })
                    .collect()
            },
        )
    }

    #[test]
    fn decode_rollup_row_handles_all_fields() {
        let row = make_row(
            vec![
                "stratum_key",
                "subsystem",
                "effect_kind",
                "risk_tier",
                "total_count",
                "resolved_count",
                "censored_count",
                "expired_count",
                "correct_count",
                "incorrect_count",
                "total_regret",
                "total_loss",
                "ewma_loss",
                "ewma_weight",
                "delay_sum_micros",
                "delay_count",
                "delay_max_micros",
                "last_updated_ts",
            ],
            vec![
                Value::Text("liveness|probe|0".to_string()),
                Value::Text("liveness".to_string()),
                Value::Text("probe".to_string()),
                Value::BigInt(0),
                Value::BigInt(100),
                Value::BigInt(90),
                Value::BigInt(3),
                Value::BigInt(7),
                Value::BigInt(85),
                Value::BigInt(5),
                Value::Double(1.5),
                Value::Double(2.0),
                Value::Double(0.02),
                Value::Double(0.95),
                Value::BigInt(5_000_000),
                Value::BigInt(90),
                Value::BigInt(120_000),
                Value::BigInt(1_713_000_000_000_000),
            ],
        );
        let entry = decode_rollup_row(&row);
        assert_eq!(entry.stratum_key, "liveness|probe|0");
        assert_eq!(entry.subsystem, "liveness");
        assert_eq!(entry.effect_kind, "probe");
        assert_eq!(entry.risk_tier, 0);
        assert_eq!(entry.total_count, 100);
        assert_eq!(entry.resolved_count, 90);
        assert_eq!(entry.censored_count, 3);
        assert_eq!(entry.expired_count, 7);
        assert_eq!(entry.correct_count, 85);
        assert_eq!(entry.incorrect_count, 5);
        assert!((entry.total_regret - 1.5).abs() < f64::EPSILON);
        assert!((entry.total_loss - 2.0).abs() < f64::EPSILON);
        assert!((entry.ewma_loss - 0.02).abs() < f64::EPSILON);
        assert!((entry.ewma_weight - 0.95).abs() < f64::EPSILON);
        assert_eq!(entry.delay_sum_micros, 5_000_000);
        assert_eq!(entry.delay_count, 90);
        assert_eq!(entry.delay_max_micros, 120_000);
    }

    #[test]
    fn decode_open_experience_handles_nulls() {
        let row = make_row(
            vec![
                "experience_id",
                "decision_id",
                "effect_id",
                "trace_id",
                "claim_id",
                "evidence_id",
                "state",
                "subsystem",
                "decision_class",
                "subject",
                "project_key",
                "policy_id",
                "effect_kind",
                "action",
                "expected_loss",
                "created_ts",
                "dispatched_ts",
                "executed_ts",
            ],
            vec![
                Value::BigInt(42),
                Value::BigInt(7),
                Value::BigInt(1),
                Value::Text("trc-abc".to_string()),
                Value::Text("clm-1".to_string()),
                Value::Text("evi-1".to_string()),
                Value::Text("open".to_string()),
                Value::Text("liveness".to_string()),
                Value::Text("probe_check".to_string()),
                Value::Text("GreenCastle".to_string()),
                Value::Null, // project_key
                Value::Null, // policy_id
                Value::Text("probe".to_string()),
                Value::Text("DeclareAlive".to_string()),
                Value::Double(0.15),
                Value::BigInt(1_000_000),
                Value::Null, // dispatched_ts
                Value::Null, // executed_ts
            ],
        );
        let entry = decode_open_experience(&row);
        assert_eq!(entry.experience_id, 42);
        assert_eq!(entry.state, "open");
        assert_eq!(entry.subject, "GreenCastle");
        assert!(entry.project_key.is_none());
        assert!(entry.dispatched_ts_micros.is_none());
        assert!(entry.executed_ts_micros.is_none());
    }

    #[test]
    fn build_query_with_no_filter() {
        let filter = OpenExperienceFilter::default();
        let (sql, params) = build_open_experience_query(&filter).expect("build query");
        assert!(sql.contains("WHERE state IN"));
        assert!(sql.contains("ORDER BY created_ts DESC"));
        assert!(!sql.contains("LIMIT"));
        assert!(params.is_empty());
    }

    #[test]
    fn build_query_with_all_filters() {
        let filter = OpenExperienceFilter {
            subject: Some("GreenCastle".to_string()),
            project_key: Some("proj-a".to_string()),
            since_ts_micros: Some(1_000_000),
            stratum_key: Some("liveness:probe:0".to_string()),
            limit: Some(50),
            ..Default::default()
        };
        let (sql, params) = build_open_experience_query(&filter).expect("build query");
        assert!(sql.contains("AND subject = ?"));
        assert!(sql.contains("AND project_key = ?"));
        assert!(sql.contains("AND created_ts >= ?"));
        assert!(sql.contains("AND subsystem = ? AND effect_kind = ?"));
        assert!(sql.contains("LIMIT 50"));
        assert_eq!(params.len(), 5);
    }

    #[test]
    fn query_open_experiences_respects_since_and_stratum_filters() {
        let pool = test_pool();
        let specs = vec![
            TestExperienceSpec {
                experience_id: 1,
                subsystem: "liveness".to_string(),
                effect_kind: "probe".to_string(),
                state: "open".to_string(),
                created_ts_micros: 1_000_000,
                resolved_ts_micros: None,
                correct: None,
                actual_loss: None,
                regret: None,
            },
            TestExperienceSpec {
                experience_id: 2,
                subsystem: "conflict".to_string(),
                effect_kind: "release".to_string(),
                state: "executed".to_string(),
                created_ts_micros: 1_500_000,
                resolved_ts_micros: None,
                correct: None,
                actual_loss: None,
                regret: None,
            },
            TestExperienceSpec {
                experience_id: 3,
                subsystem: "conflict".to_string(),
                effect_kind: "release".to_string(),
                state: "open".to_string(),
                created_ts_micros: 2_000_000,
                resolved_ts_micros: None,
                correct: None,
                actual_loss: None,
                regret: None,
            },
            TestExperienceSpec {
                experience_id: 4,
                subsystem: "conflict".to_string(),
                effect_kind: "release".to_string(),
                state: "resolved".to_string(),
                created_ts_micros: 2_100_000,
                resolved_ts_micros: Some(2_105_000),
                correct: Some(true),
                actual_loss: Some(0.0),
                regret: Some(0.0),
            },
        ];
        for spec in &specs {
            insert_experience(&pool, spec);
        }

        let rows = query_open_rows(
            &pool,
            OpenExperienceFilter {
                since_ts_micros: Some(1_400_000),
                stratum_key: Some("conflict:release:2".to_string()),
                limit: Some(10),
                ..Default::default()
            },
        );
        let ids = rows.iter().map(|row| row.experience_id).collect::<Vec<_>>();

        assert_eq!(ids, vec![3, 2]);
        assert_eq!(rows[0].state.to_string(), "open");
        assert_eq!(rows[1].state.to_string(), "executed");
        assert!(
            rows.iter()
                .all(|row| row.subsystem.to_string() == "conflict"
                    && row.effect_kind.to_string() == "release")
        );
    }

    #[test]
    fn retention_compact_deletes_old_terminal_rows_and_preserves_rollups() {
        let pool = test_pool();
        let max_age_micros = minimum_retention_compaction_age_micros().expect("policy age");
        let now = crate::now_micros();
        let old_resolved_ts = now - max_age_micros - 10_000_000;
        let old_censored_ts = now - max_age_micros - 20_000_000;
        let recent_resolved_ts = now - max_age_micros + 10_000_000;

        let specs = vec![
            TestExperienceSpec {
                experience_id: 1,
                subsystem: "liveness".to_string(),
                effect_kind: "probe".to_string(),
                state: "resolved".to_string(),
                created_ts_micros: old_resolved_ts - 5_000,
                resolved_ts_micros: Some(old_resolved_ts),
                correct: Some(true),
                actual_loss: Some(0.0),
                regret: Some(0.0),
            },
            TestExperienceSpec {
                experience_id: 2,
                subsystem: "conflict".to_string(),
                effect_kind: "release".to_string(),
                state: "censored".to_string(),
                created_ts_micros: old_censored_ts - 5_000,
                resolved_ts_micros: Some(old_censored_ts),
                correct: None,
                actual_loss: None,
                regret: None,
            },
            TestExperienceSpec {
                experience_id: 3,
                subsystem: "liveness".to_string(),
                effect_kind: "probe".to_string(),
                state: "resolved".to_string(),
                created_ts_micros: recent_resolved_ts - 5_000,
                resolved_ts_micros: Some(recent_resolved_ts),
                correct: Some(false),
                actual_loss: Some(1.0),
                regret: Some(0.5),
            },
            TestExperienceSpec {
                experience_id: 4,
                subsystem: "liveness".to_string(),
                effect_kind: "probe".to_string(),
                state: "open".to_string(),
                created_ts_micros: old_resolved_ts - 10_000,
                resolved_ts_micros: None,
                correct: None,
                actual_loss: None,
                regret: None,
            },
        ];
        for spec in &specs {
            insert_experience(&pool, spec);
        }

        run_refresh(&pool, now, now.max(1));
        let before_rollups = fetch_rollups(&pool);
        let summary = run_retention_compact(&pool, max_age_micros, true);
        let after_rollups = fetch_rollups(&pool);
        let replayed = run_replay(&pool, SequenceRange::default());
        let ids = replayed
            .rows
            .iter()
            .map(|row| row.experience_id)
            .collect::<Vec<_>>();

        assert_eq!(summary.deleted_rows, 2);
        assert!(summary.preserved_rollups);
        assert_eq!(before_rollups, after_rollups);
        assert_eq!(ids, vec![3, 4]);
    }

    #[test]
    fn refresh_rollups_can_erase_compacted_history_for_touched_stratum() {
        let pool = test_pool();
        let max_age_micros = minimum_retention_compaction_age_micros().expect("policy age");
        let now = crate::now_micros();
        let old_resolved_ts = now - max_age_micros - 10_000_000;

        let historical = TestExperienceSpec {
            experience_id: 1,
            subsystem: "liveness".to_string(),
            effect_kind: "probe".to_string(),
            state: "resolved".to_string(),
            created_ts_micros: old_resolved_ts - 5_000,
            resolved_ts_micros: Some(old_resolved_ts),
            correct: Some(true),
            actual_loss: Some(0.0),
            regret: Some(0.0),
        };
        insert_experience(&pool, &historical);

        run_refresh(&pool, now, now.max(1));
        let baseline = rollups_by_key(fetch_rollups(&pool));
        let baseline_probe = baseline
            .get("liveness:probe:0")
            .expect("baseline liveness probe rollup");
        assert_eq!(baseline_probe.total_count, 1);
        assert_eq!(baseline_probe.resolved_count, 1);

        let summary = run_retention_compact(&pool, max_age_micros, true);
        assert_eq!(summary.deleted_rows, 1);

        let after_compaction = rollups_by_key(fetch_rollups(&pool));
        let compacted_probe = after_compaction
            .get("liveness:probe:0")
            .expect("rollup should survive compaction");
        assert_eq!(compacted_probe.total_count, 1);
        assert_eq!(compacted_probe.resolved_count, 1);

        let refreshed_now = now + 20_000_000;
        let new_resolved_ts = refreshed_now - 1_000_000;
        let new_row = TestExperienceSpec {
            experience_id: 2,
            subsystem: "liveness".to_string(),
            effect_kind: "probe".to_string(),
            state: "resolved".to_string(),
            created_ts_micros: new_resolved_ts - 5_000,
            resolved_ts_micros: Some(new_resolved_ts),
            correct: Some(false),
            actual_loss: Some(1.0),
            regret: Some(0.25),
        };
        insert_experience(&pool, &new_row);

        run_refresh(&pool, refreshed_now, refreshed_now.max(1));
        let after_refresh = rollups_by_key(fetch_rollups(&pool));
        let probe = after_refresh
            .get("liveness:probe:0")
            .expect("refreshed liveness probe rollup");

        assert_eq!(
            probe.total_count, 2,
            "refresh should retain compacted historical totals and add the new row"
        );
        assert_eq!(
            probe.resolved_count, 2,
            "refresh should retain compacted resolved counts instead of recomputing from surviving raw rows only"
        );
        assert_eq!(probe.correct_count, 1);
        assert_eq!(probe.incorrect_count, 1);
        assert_eq!(probe.total_loss, 1.0);
        assert_eq!(probe.total_regret, 0.25);

        run_refresh(
            &pool,
            refreshed_now + 5_000_000,
            (refreshed_now + 5_000_000).max(1),
        );
        let after_second_refresh = rollups_by_key(fetch_rollups(&pool));
        let probe_second = after_second_refresh
            .get("liveness:probe:0")
            .expect("rollup should remain stable across repeated refreshes");
        assert_eq!(probe_second, probe);
    }

    #[test]
    fn replay_returns_rows_in_stable_sequence_order() {
        let pool = test_pool();
        let specs = vec![
            TestExperienceSpec {
                experience_id: 20,
                subsystem: "liveness".to_string(),
                effect_kind: "probe".to_string(),
                state: "open".to_string(),
                created_ts_micros: 3_000_000,
                resolved_ts_micros: None,
                correct: None,
                actual_loss: None,
                regret: None,
            },
            TestExperienceSpec {
                experience_id: 5,
                subsystem: "conflict".to_string(),
                effect_kind: "release".to_string(),
                state: "executed".to_string(),
                created_ts_micros: 9_000_000,
                resolved_ts_micros: None,
                correct: None,
                actual_loss: None,
                regret: None,
            },
            TestExperienceSpec {
                experience_id: 11,
                subsystem: "liveness".to_string(),
                effect_kind: "advisory".to_string(),
                state: "resolved".to_string(),
                created_ts_micros: 1_000_000,
                resolved_ts_micros: Some(1_005_000),
                correct: Some(true),
                actual_loss: Some(0.1),
                regret: Some(0.0),
            },
        ];
        for spec in &specs {
            insert_experience(&pool, spec);
        }

        let stream = run_replay(
            &pool,
            SequenceRange {
                from_seq: Some(5),
                to_seq: Some(20),
            },
        );
        let ids = stream
            .rows
            .iter()
            .map(|row| row.experience_id)
            .collect::<Vec<_>>();

        assert_eq!(ids, vec![5, 11, 20]);
        assert_eq!(stream.range.from_seq, Some(5));
        assert_eq!(stream.range.to_seq, Some(20));
    }

    #[test]
    fn refresh_rollups_single_row_counts_one() {
        let pool = test_pool();
        let spec = TestExperienceSpec {
            experience_id: 1,
            subsystem: "liveness".to_string(),
            effect_kind: "probe".to_string(),
            state: "resolved".to_string(),
            created_ts_micros: 1_000_000,
            resolved_ts_micros: Some(1_005_000),
            correct: Some(true),
            actual_loss: Some(0.25),
            regret: Some(0.05),
        };
        insert_experience(&pool, &spec);

        let summary = run_refresh(&pool, 1_010_000, DEFAULT_ROLLUP_REFRESH_LOOKBACK_MICROS);
        let rollups = rollups_by_key(fetch_rollups(&pool));
        let row = rollups
            .get("liveness:probe:0")
            .expect("single stratum rollup row");

        assert_eq!(summary.rows_scanned, 1);
        assert_eq!(summary.rows_applied, 1);
        assert_eq!(summary.strata_updated, 1);
        assert_eq!(row.total_count, 1);
        assert_eq!(row.resolved_count, 1);
        assert_eq!(row.correct_count, 1);
        assert_eq!(row.incorrect_count, 0);
        assert_eq!(row.delay_sum_micros, 5_000);
        assert_eq!(row.delay_count, 1);
        assert_eq!(row.delay_max_micros, 5_000);
        assert!((row.total_loss - 0.25).abs() < f64::EPSILON);
        assert!((row.total_regret - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn refresh_rollups_multi_stratum_counts_match() {
        let pool = test_pool();
        let specs = vec![
            TestExperienceSpec {
                experience_id: 1,
                subsystem: "liveness".to_string(),
                effect_kind: "probe".to_string(),
                state: "resolved".to_string(),
                created_ts_micros: 1_000_000,
                resolved_ts_micros: Some(1_005_000),
                correct: Some(true),
                actual_loss: Some(0.0),
                regret: Some(0.0),
            },
            TestExperienceSpec {
                experience_id: 2,
                subsystem: "liveness".to_string(),
                effect_kind: "probe".to_string(),
                state: "expired".to_string(),
                created_ts_micros: 1_010_000,
                resolved_ts_micros: Some(1_020_000),
                correct: None,
                actual_loss: None,
                regret: None,
            },
            TestExperienceSpec {
                experience_id: 3,
                subsystem: "deadlock".to_string(),
                effect_kind: "release".to_string(),
                state: "censored".to_string(),
                created_ts_micros: 1_030_000,
                resolved_ts_micros: Some(1_040_000),
                correct: None,
                actual_loss: None,
                regret: None,
            },
            TestExperienceSpec {
                experience_id: 4,
                subsystem: "deadlock".to_string(),
                effect_kind: "release".to_string(),
                state: "open".to_string(),
                created_ts_micros: 1_050_000,
                resolved_ts_micros: None,
                correct: None,
                actual_loss: None,
                regret: None,
            },
        ];
        for spec in &specs {
            insert_experience(&pool, spec);
        }

        run_refresh(
            &pool,
            max_touch_ts(&specs) + 1,
            DEFAULT_ROLLUP_REFRESH_LOOKBACK_MICROS,
        );
        let rollups = rollups_by_key(fetch_rollups(&pool));

        let probe = rollups.get("liveness:probe:0").expect("probe rollup");
        assert_eq!(probe.total_count, 2);
        assert_eq!(probe.resolved_count, 1);
        assert_eq!(probe.expired_count, 1);

        let release = rollups.get("deadlock:release:2").expect("release rollup");
        assert_eq!(release.total_count, 2);
        assert_eq!(release.censored_count, 1);
        assert_eq!(release.resolved_count, 0);
        assert_eq!(release.expired_count, 0);
    }

    #[test]
    fn refresh_rollups_is_idempotent() {
        let pool = test_pool();
        let specs = vec![
            TestExperienceSpec {
                experience_id: 1,
                subsystem: "liveness".to_string(),
                effect_kind: "probe".to_string(),
                state: "resolved".to_string(),
                created_ts_micros: 1_000_000,
                resolved_ts_micros: Some(1_004_000),
                correct: Some(false),
                actual_loss: Some(1.0),
                regret: Some(0.4),
            },
            TestExperienceSpec {
                experience_id: 2,
                subsystem: "liveness".to_string(),
                effect_kind: "probe".to_string(),
                state: "open".to_string(),
                created_ts_micros: 1_020_000,
                resolved_ts_micros: None,
                correct: None,
                actual_loss: None,
                regret: None,
            },
        ];
        for spec in &specs {
            insert_experience(&pool, spec);
        }

        let now_micros = max_touch_ts(&specs) + 1;
        run_refresh(&pool, now_micros, DEFAULT_ROLLUP_REFRESH_LOOKBACK_MICROS);
        let first = fetch_rollups(&pool);
        run_refresh(&pool, now_micros, DEFAULT_ROLLUP_REFRESH_LOOKBACK_MICROS);
        let second = fetch_rollups(&pool);

        assert_eq!(first, second);
    }

    #[test]
    fn refresh_rollups_window_bound_updates_only_recent_stratum() {
        let pool = test_pool();
        let old = TestExperienceSpec {
            experience_id: 1,
            subsystem: "liveness".to_string(),
            effect_kind: "probe".to_string(),
            state: "resolved".to_string(),
            created_ts_micros: 100,
            resolved_ts_micros: Some(200),
            correct: Some(true),
            actual_loss: Some(0.0),
            regret: Some(0.0),
        };
        let recent = TestExperienceSpec {
            experience_id: 2,
            subsystem: "deadlock".to_string(),
            effect_kind: "release".to_string(),
            state: "resolved".to_string(),
            created_ts_micros: 1_000_000,
            resolved_ts_micros: Some(1_010_000),
            correct: Some(false),
            actual_loss: Some(2.0),
            regret: Some(1.0),
        };
        insert_experience(&pool, &old);
        insert_experience(&pool, &recent);

        run_refresh(&pool, 1_020_000, 2_000_000);
        let baseline = rollups_by_key(fetch_rollups(&pool));

        let recent_extra = TestExperienceSpec {
            experience_id: 3,
            subsystem: "deadlock".to_string(),
            effect_kind: "release".to_string(),
            state: "censored".to_string(),
            created_ts_micros: 1_100_000,
            resolved_ts_micros: Some(1_120_000),
            correct: None,
            actual_loss: None,
            regret: None,
        };
        insert_experience(&pool, &recent_extra);

        run_refresh(&pool, 1_130_000, 200_000);
        let after = rollups_by_key(fetch_rollups(&pool));

        assert_eq!(
            baseline.get("liveness:probe:0"),
            after.get("liveness:probe:0"),
            "old untouched stratum should be preserved"
        );
        let release = after
            .get("deadlock:release:2")
            .expect("recent release stratum");
        assert_eq!(release.total_count, 2);
        assert_eq!(release.resolved_count, 1);
        assert_eq!(release.censored_count, 1);
    }

    proptest! {
        #[test]
        fn refresh_rollups_conserves_counts(specs in experience_specs_strategy()) {
            let pool = test_pool();
            for spec in &specs {
                insert_experience(&pool, spec);
            }

            let now_micros = max_touch_ts(&specs) + 1;
            let _summary = run_refresh(&pool, now_micros, now_micros.max(1));
            let actual = rollups_by_key(fetch_rollups(&pool));
            let expected = expected_counts(&specs);

            prop_assert_eq!(actual.len(), expected.len());
            for (stratum_key, expected_counts) in expected {
                let row = actual.get(&stratum_key).expect("expected stratum present");
                prop_assert_eq!(row.total_count, expected_counts.total_count);
                prop_assert_eq!(row.resolved_count, expected_counts.resolved_count);
                prop_assert_eq!(row.censored_count, expected_counts.censored_count);
                prop_assert_eq!(row.expired_count, expected_counts.expired_count);
                prop_assert_eq!(row.correct_count, expected_counts.correct_count);
                prop_assert_eq!(row.incorrect_count, expected_counts.incorrect_count);
            }
        }
    }

    // ── Expiry sweep tests ─────────────────────────────────────────

    fn make_open_summary(
        id: i64,
        effect_kind: &str,
        created_ts_micros: i64,
    ) -> OpenExperienceSummary {
        OpenExperienceSummary {
            experience_id: id,
            decision_id: 1,
            effect_id: 1,
            trace_id: format!("trc-{id}"),
            state: "open".to_string(),
            subsystem: "liveness".to_string(),
            decision_class: "probe_check".to_string(),
            subject: "GreenCastle".to_string(),
            project_key: None,
            effect_kind: effect_kind.to_string(),
            action: "DeclareAlive".to_string(),
            expected_loss: 0.1,
            created_ts_micros,
            dispatched_ts_micros: None,
            executed_ts_micros: None,
        }
    }

    #[test]
    fn find_expired_detects_overdue_probe() {
        // Probe window: 60s = 60_000_000 micros
        let created = 1_000_000;
        let now = created + 60_000_001; // 1 microsecond past window
        let open = vec![make_open_summary(1, "probe", created)];
        let expired = find_expired_experiences(&open, now);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].experience_id, 1);
        assert_eq!(expired[0].window_micros, 60_000_000);
    }

    #[test]
    fn find_expired_skips_within_window() {
        let created = 1_000_000;
        let now = created + 59_999_999; // 1 microsecond before window
        let open = vec![make_open_summary(1, "probe", created)];
        let expired = find_expired_experiences(&open, now);
        assert!(expired.is_empty());
    }

    #[test]
    fn find_expired_exact_boundary_is_expired() {
        let created = 1_000_000;
        let now = created + 60_000_000; // exactly at window edge
        let open = vec![make_open_summary(1, "probe", created)];
        let expired = find_expired_experiences(&open, now);
        assert_eq!(expired.len(), 1);
    }

    #[test]
    fn find_expired_uses_execution_anchor_when_present() {
        let created = 1_000_000;
        let executed = created + 50_000_000;
        let now = created + 100_000_000;
        let mut row = make_open_summary(1, "probe", created);
        row.executed_ts_micros = Some(executed);

        let expired = find_expired_experiences(&[row], now);

        assert!(
            expired.is_empty(),
            "probe attribution should start from execution time, not creation time"
        );
    }

    #[test]
    fn find_expired_falls_back_to_dispatch_anchor() {
        let created = 1_000_000;
        let dispatched = created + 45_000_000;
        let now = created + 100_000_000;
        let mut row = make_open_summary(1, "probe", created);
        row.dispatched_ts_micros = Some(dispatched);

        let expired = find_expired_experiences(&[row], now);

        assert!(
            expired.is_empty(),
            "probe attribution should fall back to dispatch time before creation time"
        );
    }

    #[test]
    fn find_expired_multiple_effect_kinds() {
        let base = 1_000_000;
        let now = base + 300_000_001; // past all windows
        let open = vec![
            make_open_summary(1, "probe", base),        // 60s window
            make_open_summary(2, "advisory", base),     // 300s window
            make_open_summary(3, "release", base),      // 120s window
            make_open_summary(4, "no_action", base),    // 300s window
            make_open_summary(5, "backpressure", base), // 120s window
        ];
        let expired = find_expired_experiences(&open, now);
        assert_eq!(expired.len(), 5);
    }

    #[test]
    fn find_expired_mixed_timing() {
        let now = 1_000_000_000;
        let open = vec![
            // Created 61s ago → probe expired (60s window)
            make_open_summary(1, "probe", now - 61_000_000),
            // Created 30s ago → probe NOT expired
            make_open_summary(2, "probe", now - 30_000_000),
            // Created 301s ago → advisory expired (300s window)
            make_open_summary(3, "advisory", now - 301_000_000),
            // Created 100s ago → advisory NOT expired
            make_open_summary(4, "advisory", now - 100_000_000),
        ];
        let expired = find_expired_experiences(&open, now);
        assert_eq!(expired.len(), 2);
        let ids: Vec<u64> = expired.iter().map(|e| e.experience_id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&3));
    }

    #[test]
    fn find_expired_unknown_effect_kind_is_skipped() {
        let now = 1_000_000_000;
        let open = vec![make_open_summary(1, "unknown_kind", 0)];
        let expired = find_expired_experiences(&open, now);
        assert!(expired.is_empty());
    }

    #[test]
    fn parse_effect_kind_roundtrip() {
        let cases = [
            ("advisory", EffectKind::Advisory),
            ("probe", EffectKind::Probe),
            ("release", EffectKind::Release),
            ("force_reservation", EffectKind::ForceReservation),
            ("routing_suggestion", EffectKind::RoutingSuggestion),
            ("backpressure", EffectKind::Backpressure),
            ("no_action", EffectKind::NoAction),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_effect_kind(text), Some(expected), "failed for {text}");
        }
        assert_eq!(parse_effect_kind("bogus"), None);
    }
}
