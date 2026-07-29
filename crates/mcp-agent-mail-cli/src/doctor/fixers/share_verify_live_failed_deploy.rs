//! `fm-share_export_state-verify-live-failed-deploy` — P2
//! detect-only.
//!
//! **Subsystem**: share_export_state.
//!
//! ## What's broken
//!
//! `am share verify-live` writes a `.verify-report.json` into each
//! deployed bundle dir under `<project_root>/archived_mailbox_states/`
//! after the post-deploy probe completes. The report carries a
//! `verdict` field that's either `"ok"` (everything reachable + bytes
//! match) or `"fail"` (probe couldn't reach the live URL, or the
//! deployed bytes diverged from the bundle on disk).
//!
//! A `"fail"` verdict that's been sitting for more than 1 hour
//! without a fresh successful re-run is a P2: the live deploy is
//! mismatched / unreachable but the bundle dir hasn't been
//! touched. Operators triaging from `am robot status` may otherwise
//! miss this because the bundle FILE state looks healthy
//! (manifest + payload files present and in agreement).
//!
//! Distinct from:
//!
//! - `share_half_finished_bundle` (FM23): debris from in-progress
//!   crashes (stale staged-output dirs, missing DB payload, partial zip).
//! - `share_scrub_manifest_mismatch` (FM24): manifest counts don't
//!   match on-disk attachments.
//!
//! THIS FM applies to bundles that look fully published but whose
//! POST-DEPLOY live-verification probe found a problem with the
//! remote serving the bundle (URL down, served bytes mismatched,
//! TLS chain broken, etc.).
//!
//! ## Detection (pure)
//!
//! For each immediate subdir under
//! `<project_root>/archived_mailbox_states/` (deliberately skipping
//! share-export temp dirs to avoid FM23 double-emit):
//!
//! 1. Look for `<bundle>/.verify-report.json`. If absent, skip
//!    (no probe has been run yet — out of scope).
//! 2. Parse the report. If parse fails, skip silently — a
//!    malformed report is owned elsewhere.
//! 3. If `verdict == "fail"` AND the report's mtime is older than
//!    the stale threshold (default 3600s, tunable to 0 in tests
//!    via `stale_threshold_secs_override`), record the failure.
//!
//! ## Fix
//!
//! **Detect-only by design.** Remote rollback / redeploy is
//! operator-driven; the doctor cannot mutate the live URL via
//! `mutate()`. Manual remediation: re-run `am share verify-live`
//! to confirm the failure is still real, then investigate the
//! deploy target (CDN cache, TLS, DNS, the bundle bytes) before
//! deciding whether to re-deploy or roll back.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const FM_ID: &str = "fm-share_export_state-verify-live-failed-deploy";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "share_export_state";

const ARCHIVE_STATES_DIR: &str = "archived_mailbox_states";

/// Default age threshold for a `"fail"` verdict — below this,
/// it could be from a probe currently retrying. 1 hour matches
/// the FM23 stale-temp threshold.
pub const DEFAULT_STALE_THRESHOLD_SECS: u64 = 3600;

/// Legacy temp-dir prefixes FM23 owns; skip them here.
const LEGACY_SKIP_PREFIXES: &[&str] = &[
    "am-share-finalize-rollback-",
    "am-share-finalize-",
    "am-share-stage-",
    "am-share-backup-",
];

#[derive(Debug, Clone, Serialize)]
pub struct FailedDeployEntry {
    pub bundle_dir: PathBuf,
    pub report_path: PathBuf,
    pub verdict: String,
    pub report_age_secs: u64,
    /// Optional human-readable failure summary from the report's
    /// `reason` / `error` field if present (best-effort).
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShareVerifyLiveFailedFinding {
    pub archive_root: PathBuf,
    pub stale_threshold_secs: u64,
    pub entries: Vec<FailedDeployEntry>,
}

impl ShareVerifyLiveFailedFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} share bundle(s) under {} have `verify-live` failure verdicts older than {}s",
            self.entries.len(),
            self.archive_root.display(),
            self.stale_threshold_secs,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "archive_root": self.archive_root.to_string_lossy(),
                "stale_threshold_secs": self.stale_threshold_secs,
                "entries": self.entries,
                "manual_remediation": {
                    "steps": [
                        "Re-run `am share verify-live --bundle <path>` for each entry to confirm the failure is still real (the live target may have recovered since the report was written).",
                        "If the failure persists, inspect the deploy target: CDN cache freshness (`curl -I <bundle-url>`), TLS chain (`openssl s_client -connect ...`), DNS resolution, and the on-disk bundle's manifest + zip integrity.",
                        "If the deploy target is the wrong / stale version, redeploy from the local bundle. If the local bundle is the stale one, re-export via `am share` first.",
                        "Re-run `am doctor fix --only fm-share_export_state-verify-live-failed-deploy --list` after the next successful verify-live to confirm the entry clears.",
                    ],
                    "warning": "Detect-only by design — the doctor cannot mutate a live URL via the chokepoint. Remote rollback / redeploy is always operator-driven.",
                    "common_causes": [
                        "CDN cache serving an old bundle version after a fresh redeploy (typically clears in 5-15 minutes).",
                        "TLS certificate rotated on the deploy target but the bundle's expected fingerprint wasn't updated.",
                        "Bundle bytes on disk diverged from what was uploaded (rare — usually means the upload step half-completed).",
                        "DNS or network outage at the time of the probe; report is stale once connectivity returns.",
                    ],
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DetectInputs {
    /// Override the failure-verdict age threshold (seconds).
    /// `None` uses `DEFAULT_STALE_THRESHOLD_SECS`. Test ergonomics:
    /// `Some(0)` makes any reporter age "stale" instantly so the
    /// assertion can run without backdating mtime.
    pub stale_threshold_secs_override: Option<u64>,
}

/// Detector. PURE w.r.t. the supplied project root.
pub fn detect(
    project_root: Option<&Path>,
    inputs: &DetectInputs,
) -> Vec<ShareVerifyLiveFailedFinding> {
    let Some(root) = project_root else {
        return Vec::new();
    };
    let archive_root = root.join(ARCHIVE_STATES_DIR);
    if !archive_root.is_dir() {
        return Vec::new();
    }
    let stale_threshold_secs = inputs
        .stale_threshold_secs_override
        .unwrap_or(DEFAULT_STALE_THRESHOLD_SECS);
    let now = SystemTime::now();
    let mut entries: Vec<FailedDeployEntry> = Vec::new();
    let Ok(rd) = std::fs::read_dir(&archive_root) else {
        return Vec::new();
    };
    for entry in rd.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let bundle_dir = entry.path();
        if let Some(name) = bundle_dir.file_name().and_then(|n| n.to_str())
            && is_share_temp_dir_name(name)
        {
            continue;
        }
        check_bundle(&bundle_dir, now, stale_threshold_secs, &mut entries);
    }
    if entries.is_empty() {
        return Vec::new();
    }
    vec![ShareVerifyLiveFailedFinding {
        archive_root,
        stale_threshold_secs,
        entries,
    }]
}

fn is_share_temp_dir_name(name: &str) -> bool {
    LEGACY_SKIP_PREFIXES.iter().any(|p| name.starts_with(p))
        || name.starts_with(".bundle-zip.")
        || name.starts_with("mailbox-archive-zip-")
        || name.contains(".bundle-stage.")
        || name.contains(".bundle-backup.")
}

fn check_bundle(
    bundle_dir: &Path,
    now: SystemTime,
    stale_threshold_secs: u64,
    out: &mut Vec<FailedDeployEntry>,
) {
    let report_path = bundle_dir.join(".verify-report.json");
    if !report_path.is_file() {
        return;
    }
    let Ok(body) = std::fs::read_to_string(&report_path) else {
        return;
    };
    let Ok(report) = serde_json::from_str::<serde_json::Value>(&body) else {
        return;
    };
    let verdict = report.get("verdict").and_then(|v| v.as_str()).unwrap_or("");
    if verdict != "fail" {
        return;
    }
    let Ok(report_meta) = std::fs::metadata(&report_path) else {
        return;
    };
    let Ok(report_mtime) = report_meta.modified() else {
        return;
    };
    let report_age_secs = now
        .duration_since(report_mtime)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if report_age_secs < stale_threshold_secs {
        return;
    }
    let failure_reason = report
        .get("reason")
        .and_then(|v| v.as_str())
        .or_else(|| report.get("error").and_then(|v| v.as_str()))
        .map(str::to_string);
    out.push(FailedDeployEntry {
        bundle_dir: bundle_dir.to_path_buf(),
        report_path,
        verdict: verdict.to_string(),
        report_age_secs,
        failure_reason,
    });
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &ShareVerifyLiveFailedFinding,
) -> Result<FixOutcome, MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_archive(td: &TempDir) -> PathBuf {
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        fs::create_dir_all(&arch).unwrap();
        arch
    }

    fn write_report(bundle: &Path, body: &str) {
        fs::create_dir_all(bundle).unwrap();
        fs::write(bundle.join(".verify-report.json"), body).unwrap();
    }

    fn zero_threshold() -> DetectInputs {
        DetectInputs {
            stale_threshold_secs_override: Some(0),
        }
    }

    /// **NEGATIVE TEST FIRST**: no project root → no finding.
    #[test]
    fn detector_returns_empty_for_no_project_root() {
        assert!(detect(None, &DetectInputs::default()).is_empty());
    }

    /// **NEGATIVE**: archive dir absent → no finding.
    #[test]
    fn detector_returns_empty_when_archive_dir_absent() {
        let td = TempDir::new().unwrap();
        assert!(detect(Some(td.path()), &DetectInputs::default()).is_empty());
    }

    /// **NEGATIVE**: bundle has no verify report → out of scope.
    #[test]
    fn detector_skips_bundle_without_verify_report() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(&bundle).unwrap();
        assert!(detect(Some(td.path()), &zero_threshold()).is_empty());
    }

    /// **NEGATIVE**: a healthy verdict ("ok") must NOT flag.
    #[test]
    fn detector_skips_ok_verdict() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        write_report(&bundle, r#"{"verdict":"ok"}"#);
        assert!(detect(Some(td.path()), &zero_threshold()).is_empty());
    }

    /// **NEGATIVE**: malformed report → skip silently.
    #[test]
    fn detector_skips_malformed_report() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        write_report(&bundle, "not json at all");
        assert!(detect(Some(td.path()), &zero_threshold()).is_empty());
    }

    /// **NEGATIVE**: fresh `fail` verdict (within stale threshold) →
    /// NOT flagged. The default 3600s threshold makes a live-just-
    /// written report invisible to the detector.
    #[test]
    fn detector_skips_fresh_failure_verdict_under_default_threshold() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        write_report(&bundle, r#"{"verdict":"fail","reason":"flaky probe"}"#);
        let findings = detect(Some(td.path()), &DetectInputs::default());
        assert!(
            findings.is_empty(),
            "fresh fail verdict must not flag with default threshold"
        );
    }

    /// **NEGATIVE**: skip share-export temp dirs (FM23 territory).
    #[test]
    fn detector_skips_share_export_temp_dirs() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join(".bundle.bundle-stage.123.0");
        write_report(&bundle, r#"{"verdict":"fail"}"#);
        assert!(detect(Some(td.path()), &zero_threshold()).is_empty());
    }

    #[test]
    fn detector_flags_failed_verdict_past_threshold() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        write_report(
            &bundle,
            r#"{"verdict":"fail","reason":"CDN cache stale; remote bytes differ"}"#,
        );
        let findings = detect(Some(td.path()), &zero_threshold());
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].verdict, "fail");
        assert_eq!(
            f.entries[0].failure_reason.as_deref(),
            Some("CDN cache stale; remote bytes differ")
        );
        assert!(f.entries[0].bundle_dir.ends_with("bundle-1"));
    }

    /// Failure reason also accepted from `error` field as a fallback.
    #[test]
    fn detector_reads_failure_reason_from_error_field_fallback() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        write_report(&bundle, r#"{"verdict":"fail","error":"TLS chain broken"}"#);
        let findings = detect(Some(td.path()), &zero_threshold());
        assert_eq!(
            findings[0].entries[0].failure_reason.as_deref(),
            Some("TLS chain broken")
        );
    }

    #[test]
    fn detector_aggregates_multiple_failed_bundles_in_one_finding() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        for i in 0..3 {
            let bundle = arch.join(format!("bundle-{i}"));
            write_report(&bundle, r#"{"verdict":"fail"}"#);
        }
        let findings = detect(Some(td.path()), &zero_threshold());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn detector_does_not_follow_symlinked_archive_subdir() {
        use std::os::unix::fs::symlink;

        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let outside = td.path().join("outside-bundle");
        write_report(&outside, r#"{"verdict":"fail","reason":"outside"}"#);
        symlink(&outside, arch.join("linked-outside")).unwrap();

        assert!(
            detect(Some(td.path()), &zero_threshold()).is_empty(),
            "verify-live detector must not escape through symlinked bundle dirs"
        );
    }

    #[test]
    fn detector_records_threshold_for_evidence_transparency() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        write_report(&bundle, r#"{"verdict":"fail"}"#);
        let findings = detect(
            Some(td.path()),
            &DetectInputs {
                stale_threshold_secs_override: Some(0),
            },
        );
        assert_eq!(findings[0].stale_threshold_secs, 0);
    }

    #[test]
    fn finding_serializes_with_verdict_strings_and_remediation() {
        let f = ShareVerifyLiveFailedFinding {
            archive_root: "/tmp/x".into(),
            stale_threshold_secs: 3600,
            entries: vec![FailedDeployEntry {
                bundle_dir: "/tmp/x/bundle-1".into(),
                report_path: "/tmp/x/bundle-1/.verify-report.json".into(),
                verdict: "fail".to_string(),
                report_age_secs: 7200,
                failure_reason: Some("CDN cache".to_string()),
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"verdict\":\"fail\""));
        assert!(s.contains("\"stale_threshold_secs\":3600"));
        assert!(s.contains("am share verify-live"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        let td = TempDir::new().unwrap();
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), "test_run").unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = MutateContext {
            run_id: "test_run".into(),
            run_dir,
            capabilities: crate::doctor::mutate::Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: std::sync::Mutex::new(actions),
            fixer_id: FM_ID.into(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: std::time::Instant::now(),
            extra_locks: Vec::new(),
        };
        let finding = ShareVerifyLiveFailedFinding {
            archive_root: td.path().to_path_buf(),
            stale_threshold_secs: 3600,
            entries: Vec::new(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
