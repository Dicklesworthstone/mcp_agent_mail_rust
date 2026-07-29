//! `fm-db-state-files-recovery-breaker-tripped` — P1, detect-only.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! The durable recovery circuit breaker (br-acusl) parked automatic
//! recovery for a database: N consecutive automatic recovery attempts
//! failed on the SAME database content, so the self-heal loop stopped
//! re-attempting (and stopped capturing a forensic bundle per attempt —
//! the trj incident refilled 96 GB of dumps that way). The mailbox is
//! degraded until an operator intervenes: reads may work, but corruption
//! recovery will not be retried automatically until the cooldown elapses
//! or the database content changes.
//!
//! ## Detection (pure function)
//!
//! For each candidate database path: read the
//! `<db>.am-recovery-breaker.json` sidecar; emit a finding when it is
//! `tripped` AND its recorded content fingerprint still matches the
//! database currently on disk (a fingerprint mismatch means the content
//! already changed — the breaker will admit the next attempt on its own,
//! so there is nothing to surface).
//!
//! ## Fix
//!
//! **Detect-only by design.** The whole point of the breaker is that
//! automatic remediation did not converge; only operator-supplied truth
//! can resolve it. The remediation envelope names the exact commands:
//! `am doctor repair`, `am doctor reconstruct` (both bypass the breaker
//! and clear it on success), or quarantining the database file to rebuild
//! from the Git archive.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-db-state-files-recovery-breaker-tripped";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "db_state_files";

#[derive(Debug, Clone, Serialize)]
pub struct RecoveryBreakerTrippedFinding {
    pub db_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub consecutive_failures: u32,
    pub last_failure_unix: i64,
    pub last_failure_reason: String,
}

impl RecoveryBreakerTrippedFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "automatic recovery for {} is circuit-broken after {} consecutive failures on the same database content",
            self.db_path.display(),
            self.consecutive_failures,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "sidecar_path": self.sidecar_path.to_string_lossy(),
                "consecutive_failures": self.consecutive_failures,
                "last_failure_unix": self.last_failure_unix,
                "last_failure_reason": self.last_failure_reason,
                "operator_remediation": [
                    "am doctor repair       # bypasses the breaker; clears it on success",
                    "am doctor reconstruct  # archive-first rebuild; bypasses the breaker",
                    "quarantine the file (move storage.sqlite3* aside) to rebuild from the Git archive; the breaker admits changed content automatically",
                ],
                "cooldown_note": "automatic recovery half-opens on its own after AM_RECOVERY_BREAKER_COOLDOWN_SECS (default 21600s)",
            }),
            remediation: FindingRemediation {
                command: "am doctor repair".to_string(),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Detector. PURE (filesystem reads only).
pub fn detect(db_file_candidates: &[PathBuf]) -> Vec<RecoveryBreakerTrippedFinding> {
    let mut findings = Vec::new();
    for db_path in db_file_candidates {
        if let Some(finding) = detect_one(db_path) {
            findings.push(finding);
        }
    }
    findings.sort_by(|a, b| a.db_path.cmp(&b.db_path));
    findings
}

fn detect_one(db_path: &Path) -> Option<RecoveryBreakerTrippedFinding> {
    let state = mcp_agent_mail_db::recovery_breaker::load(db_path)?;
    if !state.tripped {
        return None;
    }
    if state.db_fingerprint != mcp_agent_mail_db::recovery_breaker::fingerprint_db(db_path) {
        // Content already changed (operator intervened / file replaced):
        // the breaker will admit the next attempt on its own.
        return None;
    }
    Some(RecoveryBreakerTrippedFinding {
        db_path: db_path.to_path_buf(),
        sidecar_path: mcp_agent_mail_db::recovery_breaker::breaker_sidecar_path(db_path),
        consecutive_failures: state.consecutive_failures,
        last_failure_unix: state.last_failure_unix,
        last_failure_reason: state.last_failure_reason,
    })
}

/// Fixer. Detect-only — resolution needs operator-supplied truth.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &RecoveryBreakerTrippedFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_db::recovery_breaker::{
        RecoveryBreakerConfig, cleared_state, fingerprint_db, record_failure, store,
    };
    use tempfile::TempDir;

    const CFG: RecoveryBreakerConfig = RecoveryBreakerConfig {
        max_consecutive_failures: 3,
        cooldown_secs: 100,
    };

    fn tripped_db(td: &TempDir, name: &str) -> PathBuf {
        let db = td.path().join(name);
        std::fs::write(&db, b"stuck-content").unwrap();
        let fingerprint = fingerprint_db(&db);
        let mut state = record_failure(None, &fingerprint, "boom", CFG, 10);
        state = record_failure(Some(&state), &fingerprint, "boom", CFG, 20);
        state = record_failure(Some(&state), &fingerprint, "boom again", CFG, 30);
        assert!(state.tripped);
        store(&db, &state);
        db
    }

    #[test]
    fn detector_returns_empty_without_sidecar_or_when_not_tripped() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        assert!(
            detect(std::slice::from_ref(&db)).is_empty(),
            "no sidecar → no finding"
        );

        store(&db, &cleared_state(&fingerprint_db(&db)));
        assert!(
            detect(&[db]).is_empty(),
            "an un-tripped sidecar must not flag"
        );
    }

    #[test]
    fn detector_flags_tripped_breaker_with_matching_content() {
        let td = TempDir::new().unwrap();
        let db = tripped_db(&td, "storage.sqlite3");
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].db_path, db);
        assert_eq!(findings[0].consecutive_failures, 3);
        assert_eq!(findings[0].last_failure_reason, "boom again");
    }

    #[test]
    fn detector_stays_quiet_once_content_changed() {
        let td = TempDir::new().unwrap();
        let db = tripped_db(&td, "storage.sqlite3");
        std::fs::write(&db, b"operator-replaced-content").unwrap();
        assert!(
            detect(&[db]).is_empty(),
            "changed content self-resolves; the breaker admits the next attempt"
        );
    }

    #[test]
    fn finding_serializes_with_required_fields() {
        let finding = RecoveryBreakerTrippedFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            sidecar_path: PathBuf::from("/x/storage.sqlite3.am-recovery-breaker.json"),
            consecutive_failures: 4,
            last_failure_unix: 99,
            last_failure_reason: "reconstruct failed".to_string(),
        };
        let generic = finding.to_finding();
        assert_eq!(generic.id, FM_ID);
        assert_eq!(generic.severity, "P1");
        assert_eq!(generic.subsystem, "db_state_files");
        assert!(!generic.remediation.auto_fixable);
        let serialized = serde_json::to_string(&generic).unwrap();
        assert!(serialized.contains(FM_ID));
        assert!(serialized.contains("am doctor repair"));
        assert!(serialized.contains("reconstruct failed"));
    }
}
