//! Minimal evidence-ledger primitives for explainable runtime decisions.
//!
//! This module provides append-only JSONL emission with an opt-in path
//! configured through `AM_EVIDENCE_LEDGER_PATH`.

use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Optional JSONL file path for decision-evidence emission.
///
/// When unset or blank, evidence emission is disabled and callers receive
/// `Ok(false)` from [`append_evidence_entry_if_configured`].
pub const EVIDENCE_LEDGER_PATH_ENV: &str = "AM_EVIDENCE_LEDGER_PATH";

static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// A single decision record in the evidence ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceLedgerEntry {
    /// Wall-clock timestamp in microseconds since Unix epoch.
    pub ts_micros: i64,
    /// Stable decision identifier for correlation across traces.
    pub decision_id: String,
    /// Logical decision point (e.g., `search.hybrid_budget`).
    pub decision_point: String,
    /// Chosen action label.
    pub action: String,
    /// Confidence score in the chosen action, in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Structured evidence payload that explains the decision context.
    pub evidence: Value,
    /// Expected loss associated with the chosen action (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_loss: Option<f64>,
    /// Optional expected outcome string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// Optional actual outcome string (for later backfill).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    /// Optional correctness marker (for later backfill).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correct: Option<bool>,
    /// Optional request/trace correlation id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

impl EvidenceLedgerEntry {
    /// Construct a new evidence entry with the current timestamp.
    #[must_use]
    pub fn new(
        decision_id: impl Into<String>,
        decision_point: impl Into<String>,
        action: impl Into<String>,
        confidence: f64,
        evidence: Value,
    ) -> Self {
        Self {
            ts_micros: Utc::now().timestamp_micros(),
            decision_id: decision_id.into(),
            decision_point: decision_point.into(),
            action: action.into(),
            confidence,
            evidence,
            expected_loss: None,
            expected: None,
            actual: None,
            correct: None,
            trace_id: None,
        }
    }
}

fn parse_configured_path(raw: Option<&str>) -> Option<PathBuf> {
    let raw = raw?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn configured_path() -> Option<PathBuf> {
    parse_configured_path(std::env::var(EVIDENCE_LEDGER_PATH_ENV).ok().as_deref())
}

fn with_write_lock<F, T>(f: F) -> T
where
    F: FnOnce() -> T,
{
    let lock = WRITE_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f()
}

/// Append an evidence entry to the configured JSONL file.
///
/// Returns:
/// - `Ok(true)` when a record was written
/// - `Ok(false)` when emission is disabled (`AM_EVIDENCE_LEDGER_PATH` unset)
/// - `Err(_)` on I/O or serialization failures
pub fn append_evidence_entry_if_configured(entry: &EvidenceLedgerEntry) -> io::Result<bool> {
    let Some(path) = configured_path() else {
        return Ok(false);
    };
    append_evidence_entry_to_path(&path, entry)?;
    Ok(true)
}

/// Append an evidence entry to a specific JSONL file path.
///
/// Parent directories are created automatically.
pub fn append_evidence_entry_to_path(path: &Path, entry: &EvidenceLedgerEntry) -> io::Result<()> {
    with_write_lock(|| -> io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, entry).map_err(io::Error::other)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::thread;

    use tempfile::tempdir;

    #[test]
    fn parse_configured_path_rejects_blank_values() {
        assert_eq!(parse_configured_path(None), None);
        assert_eq!(parse_configured_path(Some("")), None);
        assert_eq!(parse_configured_path(Some("   ")), None);
    }

    #[test]
    fn parse_configured_path_accepts_trimmed_path() {
        let parsed = parse_configured_path(Some("  /tmp/evidence.jsonl  "));
        assert_eq!(parsed, Some(PathBuf::from("/tmp/evidence.jsonl")));
    }

    #[test]
    fn append_to_path_writes_line_delimited_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let entry = EvidenceLedgerEntry::new(
            "decision-1",
            "search.hybrid_budget",
            "semantic_dominant",
            0.91,
            serde_json::json!({"query":"how to tune search"}),
        );
        append_evidence_entry_to_path(&path, &entry).unwrap();

        let content = std::fs::read_to_string(path).unwrap();
        let lines = content.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let decoded: EvidenceLedgerEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(decoded.decision_id, "decision-1");
        assert_eq!(decoded.action, "semantic_dominant");
    }

    #[test]
    fn append_to_path_creates_parent_directories() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("nested/audit/ledger.jsonl");
        let entry = EvidenceLedgerEntry::new(
            "decision-2",
            "search.hybrid_budget",
            "balanced",
            0.67,
            serde_json::json!({"mode":"auto"}),
        );
        append_evidence_entry_to_path(&nested, &entry).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn concurrent_appends_keep_all_records() {
        let dir = tempdir().unwrap();
        let path = Arc::new(dir.path().join("ledger.jsonl"));

        let mut handles = Vec::new();
        for worker in 0..8 {
            let path = Arc::clone(&path);
            handles.push(thread::spawn(move || {
                for idx in 0..25 {
                    let entry = EvidenceLedgerEntry::new(
                        format!("d-{worker}-{idx}"),
                        "search.hybrid_budget",
                        "balanced",
                        0.5,
                        serde_json::json!({"worker":worker,"idx":idx}),
                    );
                    append_evidence_entry_to_path(path.as_path(), &entry).unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let line_count = std::fs::read_to_string(path.as_path())
            .unwrap()
            .lines()
            .count();
        assert_eq!(line_count, 200);
    }
}
