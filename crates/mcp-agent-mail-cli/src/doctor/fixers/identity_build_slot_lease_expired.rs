//! `fm-identity_contacts_state-build-slot-lease-expired` — P2
//! detect-only.
//!
//! **Subsystem**: identity_contacts_state.
//!
//! ## What's broken
//!
//! Each acquired build slot writes a JSON lease artifact to
//! `<storage_root>/projects/<slug>/build_slots/<slot>/<lease>.json`.
//! The lease records `acquired_ts`, `expires_ts`, and (when the
//! slot is released cleanly) `released_ts`. Live leases have
//! `released_ts == null` and `expires_ts` in the future.
//!
//! When an agent crashes / OOMs / network-partitions before
//! calling `release_build_slot`, the lease remains on disk with
//! `released_ts == null` past its `expires_ts`. This is a P2:
//!
//! - Status/ops views that inspect lease artifacts can show the
//!   slot as occupied even though nobody is running.
//! - Operators looking directly at the archive see ghost leases
//!   and wonder why builds aren't picking up.
//! - The Rust `acquire_build_slot` path ignores expired leases
//!   for conflict checks, so this is primarily an audit/UI
//!   integrity signal rather than a current availability blocker.
//!
//! ## Detection (pure)
//!
//! Walks `<storage_root>/projects/*/build_slots/*/*.json` (three
//! levels of `read_dir`), parses each `.json` file with
//! `serde_json::Value`, and records entries where:
//!
//! - `released_ts` is missing OR `null`, AND
//! - `expires_ts` parses as RFC3339 and is earlier than `now_iso`.
//!
//! Returns one aggregated finding listing each expired lease.
//!
//! ## Fix
//!
//! **Detect-only (first cut).** The repair spec calls for an
//! `Op::WriteFile` rewrite of each expired lease's `released_ts`
//! to the current ISO string (UPDATE-only — never delete per
//! RULE 1). That needs a per-lease round-trip test and careful
//! preservation of any sibling JSON fields the spec doesn't list.
//! Deferred. Manual remediation: if the holder is alive, renew
//! the lease; if the holder is dead, acquire the slot normally
//! for new work. Do not delete lease files manually — use a
//! future doctor fixer or a targeted lease rewrite that preserves
//! all sibling JSON fields.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-identity_contacts_state-build-slot-lease-expired";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "identity_contacts_state";

#[derive(Debug, Clone, Serialize)]
pub struct ExpiredLeaseEntry {
    pub lease_path: PathBuf,
    pub project_slug: String,
    pub slot_name: String,
    /// ISO-8601 from the lease's `acquired_ts` field, when present.
    pub acquired_ts: Option<String>,
    /// ISO-8601 from the lease's `expires_ts` field.
    pub expires_ts: String,
    /// Holder identifier from the lease's `holder`, `agent`, or
    /// legacy `agent_name` field, when present, for operator
    /// triage.
    pub holder: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdentityBuildSlotLeaseExpiredFinding {
    pub entries: Vec<ExpiredLeaseEntry>,
    /// The `now_iso` value the detector used. Recorded for
    /// operator-side reproducibility: an operator running the
    /// same detector seconds later may see fewer entries (if a
    /// holder released between probes) — comparing this value
    /// against the operator's wall clock makes the diff
    /// explainable.
    pub now_iso: String,
}

impl IdentityBuildSlotLeaseExpiredFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} build slot lease(s) past their expires_ts with no released_ts (ghost leases)",
            self.entries.len(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "entries": self.entries,
                "now_iso": self.now_iso,
                "manual_remediation": {
                    "steps": [
                        "For each entry: confirm the holder is actually dead before clearing the lease. If the holder is alive but past expires_ts, the right fix is to extend the lease (`renew_build_slot`), not release it.",
                        "If the holder is confirmed dead: current Rust `acquire_build_slot` already ignores expired leases for conflict checks, so new work can acquire the slot normally.",
                        "To clear the audit/UI ghost itself, rewrite only that lease's `released_ts` through a future doctor fixer or equivalent targeted build-slot maintenance path that preserves all sibling fields.",
                        "Re-run `am doctor fix --only fm-identity_contacts_state-build-slot-lease-expired --list` to confirm zero residual ghost leases.",
                    ],
                    "warning": "Do NOT delete lease files manually — the chokepoint and build-slot tooling rely on lease files as the canonical record. The Rust acquire path ignores expired leases; this FM is about stale audit/UI state, not a forced-release command.",
                    "safe_fix_deferred": "Auto-fix via `Op::WriteFile` rewriting `released_ts` to `now_iso` (UPDATE-only — never delete per RULE 1) is intentionally deferred in this first cut. Each lease JSON has sibling fields the spec doesn't enumerate; faithful round-trip preservation needs a per-lease test fixture.",
                    "common_causes": [
                        "Agent crashed / OOMed / network-partitioned without calling `release_build_slot`.",
                        "`am serve` SIGTERM'd mid-build-slot lifecycle (the cleanup_pane_identities pass may not have run).",
                        "Manual `kill` of an agent holding a long lease.",
                        "Network outage during release request (the agent thinks it released; the server never got the call).",
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
    /// Override the "now" RFC3339 string used for the expiry
    /// comparison. Test ergonomics: pin a
    /// known wall-clock so the assertion doesn't race against
    /// `SystemTime::now()`. `None` uses the actual current time.
    pub now_iso_override: Option<String>,
}

/// Detector. PURE w.r.t. the supplied `storage_root`.
pub fn detect(
    storage_root: Option<&Path>,
    inputs: &DetectInputs,
) -> Vec<IdentityBuildSlotLeaseExpiredFinding> {
    let Some(root) = storage_root else {
        return Vec::new();
    };
    let projects_dir = root.join("projects");
    if !projects_dir.is_dir() {
        return Vec::new();
    }
    let now_iso = inputs
        .now_iso_override
        .clone()
        .unwrap_or_else(now_iso_string);
    let now = chrono::DateTime::parse_from_rfc3339(&now_iso)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());

    let mut entries: Vec<ExpiredLeaseEntry> = Vec::new();
    let Ok(rd_projects) = std::fs::read_dir(&projects_dir) else {
        return Vec::new();
    };
    for project_de in rd_projects.flatten() {
        let project_path = project_de.path();
        if !project_path.is_dir() {
            continue;
        }
        let project_slug = project_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .unwrap_or_default();
        let bs_root = project_path.join("build_slots");
        if !bs_root.is_dir() {
            continue;
        }
        let Ok(rd_slots) = std::fs::read_dir(&bs_root) else {
            continue;
        };
        for slot_de in rd_slots.flatten() {
            let slot_path = slot_de.path();
            if !slot_path.is_dir() {
                continue;
            }
            let slot_name = slot_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
                .unwrap_or_default();
            let Ok(rd_leases) = std::fs::read_dir(&slot_path) else {
                continue;
            };
            for lease_de in rd_leases.flatten() {
                let lease_path = lease_de.path();
                if lease_path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Some(entry) = inspect_lease(&lease_path, &project_slug, &slot_name, now) {
                    entries.push(entry);
                }
            }
        }
    }
    if entries.is_empty() {
        return Vec::new();
    }
    vec![IdentityBuildSlotLeaseExpiredFinding { entries, now_iso }]
}

fn inspect_lease(
    lease_path: &Path,
    project_slug: &str,
    slot_name: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ExpiredLeaseEntry> {
    let body = std::fs::read_to_string(lease_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    // released_ts must be missing or null for a "live" lease.
    let released = v.get("released_ts");
    if released.is_some_and(|r| !r.is_null()) {
        return None;
    }
    let expires_ts = v.get("expires_ts").and_then(|e| e.as_str())?;
    let Ok(expires_at) =
        chrono::DateTime::parse_from_rfc3339(expires_ts).map(|dt| dt.with_timezone(&chrono::Utc))
    else {
        return None;
    };
    if expires_at >= now {
        return None;
    }
    let acquired_ts = v
        .get("acquired_ts")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let holder = v
        .get("holder")
        .or_else(|| v.get("agent"))
        .or_else(|| v.get("agent_name"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    Some(ExpiredLeaseEntry {
        lease_path: lease_path.to_path_buf(),
        project_slug: project_slug.to_string(),
        slot_name: slot_name.to_string(),
        acquired_ts,
        expires_ts: expires_ts.to_string(),
        holder,
    })
}

/// Cheap ISO-8601 generator. We don't need sub-second precision
/// for comparing against minute-resolution `expires_ts` strings;
/// the canonical "now" format mirrors what build-slot leases use
/// when they write their own `expires_ts` via `chrono::Utc::now()`.
fn now_iso_string() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &IdentityBuildSlotLeaseExpiredFinding,
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

    fn make_storage_root(td: &TempDir) -> PathBuf {
        let root = td.path().to_path_buf();
        fs::create_dir_all(root.join("projects")).unwrap();
        root
    }

    fn write_lease(
        storage_root: &Path,
        project: &str,
        slot: &str,
        lease_name: &str,
        body: &str,
    ) -> PathBuf {
        let dir = storage_root
            .join("projects")
            .join(project)
            .join("build_slots")
            .join(slot);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{lease_name}.json"));
        fs::write(&p, body).unwrap();
        p
    }

    fn now_override(now: &str) -> DetectInputs {
        DetectInputs {
            now_iso_override: Some(now.to_string()),
        }
    }

    /// **NEGATIVE TEST FIRST**: no storage_root → no finding.
    #[test]
    fn detector_returns_empty_for_no_storage_root() {
        assert!(detect(None, &DetectInputs::default()).is_empty());
    }

    /// **NEGATIVE**: storage_root has no `projects/` subdir.
    #[test]
    fn detector_returns_empty_when_projects_dir_absent() {
        let td = TempDir::new().unwrap();
        assert!(detect(Some(td.path()), &DetectInputs::default()).is_empty());
    }

    /// **NEGATIVE**: lease with future `expires_ts` → not flagged.
    #[test]
    fn detector_skips_live_lease_with_future_expiry() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        write_lease(
            &root,
            "demo",
            "build-1",
            "lease",
            r#"{"expires_ts":"2099-01-01T00:00:00Z","released_ts":null}"#,
        );
        let findings = detect(Some(&root), &now_override("2026-05-16T00:00:00Z"));
        assert!(findings.is_empty());
    }

    /// **NEGATIVE**: lease with `released_ts` populated → not
    /// flagged regardless of expires_ts.
    #[test]
    fn detector_skips_lease_that_was_released() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        write_lease(
            &root,
            "demo",
            "build-1",
            "lease",
            r#"{"expires_ts":"2020-01-01T00:00:00Z","released_ts":"2020-01-02T00:00:00Z"}"#,
        );
        let findings = detect(Some(&root), &now_override("2026-05-16T00:00:00Z"));
        assert!(findings.is_empty());
    }

    /// **NEGATIVE**: malformed JSON → skipped silently.
    #[test]
    fn detector_skips_malformed_lease_json() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        write_lease(&root, "demo", "build-1", "lease", "not json at all");
        let findings = detect(Some(&root), &now_override("2026-05-16T00:00:00Z"));
        assert!(findings.is_empty());
    }

    /// **NEGATIVE**: non-`.json` file in the lease dir → skipped.
    #[test]
    fn detector_skips_non_json_entries() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        let dir = root
            .join("projects")
            .join("demo")
            .join("build_slots")
            .join("build-1");
        fs::create_dir_all(&dir).unwrap();
        // Plant a junk file the detector should ignore.
        fs::write(dir.join("README.md"), b"docs go here").unwrap();
        let findings = detect(Some(&root), &now_override("2026-05-16T00:00:00Z"));
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_flags_expired_lease_with_null_released_ts() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        write_lease(
            &root,
            "demo",
            "build-1",
            "AlphaWaterfall",
            r#"{"expires_ts":"2026-05-15T00:00:00Z","released_ts":null,"acquired_ts":"2026-05-14T00:00:00Z","holder":"AlphaWaterfall"}"#,
        );
        let findings = detect(Some(&root), &now_override("2026-05-16T00:00:00Z"));
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.entries.len(), 1);
        let e = &f.entries[0];
        assert_eq!(e.project_slug, "demo");
        assert_eq!(e.slot_name, "build-1");
        assert_eq!(e.expires_ts, "2026-05-15T00:00:00Z");
        assert_eq!(e.acquired_ts.as_deref(), Some("2026-05-14T00:00:00Z"));
        assert_eq!(e.holder.as_deref(), Some("AlphaWaterfall"));
    }

    /// `released_ts` MISSING is the same as `released_ts: null`
    /// — flagged when expired.
    #[test]
    fn detector_flags_expired_lease_with_missing_released_ts() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        write_lease(
            &root,
            "demo",
            "build-1",
            "lease",
            r#"{"expires_ts":"2026-05-15T00:00:00Z"}"#,
        );
        let findings = detect(Some(&root), &now_override("2026-05-16T00:00:00Z"));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 1);
    }

    /// Holder can come from either `holder` OR `agent_name`
    /// (some lease shapes use the latter).
    #[test]
    fn detector_reads_holder_from_agent_name_fallback() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        write_lease(
            &root,
            "demo",
            "build-1",
            "lease",
            r#"{"expires_ts":"2026-05-15T00:00:00Z","agent_name":"BravoMountain"}"#,
        );
        let findings = detect(Some(&root), &now_override("2026-05-16T00:00:00Z"));
        assert_eq!(
            findings[0].entries[0].holder.as_deref(),
            Some("BravoMountain")
        );
    }

    #[test]
    fn detector_reads_holder_from_current_agent_field() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        write_lease(
            &root,
            "demo",
            "build-1",
            "lease",
            r#"{"expires_ts":"2026-05-15T00:00:00Z","agent":"GreenCastle"}"#,
        );
        let findings = detect(Some(&root), &now_override("2026-05-16T00:00:00Z"));
        assert_eq!(
            findings[0].entries[0].holder.as_deref(),
            Some("GreenCastle")
        );
    }

    #[test]
    fn detector_aggregates_multiple_projects_and_slots() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        // 2 projects × 2 slots × 1 expired lease each = 4 entries.
        for proj in ["p1", "p2"] {
            for slot in ["s1", "s2"] {
                write_lease(
                    &root,
                    proj,
                    slot,
                    "lease",
                    r#"{"expires_ts":"2020-01-01T00:00:00Z","released_ts":null}"#,
                );
            }
        }
        let findings = detect(Some(&root), &now_override("2026-05-16T00:00:00Z"));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 4);
    }

    /// Sanity: expiration comparison must parse RFC3339 offsets
    /// instead of relying on lexical ordering.
    #[test]
    fn detector_parses_rfc3339_offsets_before_comparing() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        write_lease(
            &root,
            "demo",
            "build-1",
            "lease",
            r#"{"expires_ts":"2026-05-15T23:30:00-01:00","released_ts":null}"#,
        );
        let findings = detect(Some(&root), &now_override("2026-05-16T00:00:00Z"));
        assert!(
            findings.is_empty(),
            "offset-bearing future lease must not be flagged as expired"
        );
    }

    #[test]
    fn finding_serializes_with_now_iso_and_remediation() {
        let f = IdentityBuildSlotLeaseExpiredFinding {
            entries: vec![ExpiredLeaseEntry {
                lease_path: "/tmp/lease.json".into(),
                project_slug: "demo".to_string(),
                slot_name: "build-1".to_string(),
                acquired_ts: Some("2026-05-14T00:00:00Z".to_string()),
                expires_ts: "2026-05-15T00:00:00Z".to_string(),
                holder: Some("AlphaWaterfall".to_string()),
            }],
            now_iso: "2026-05-16T00:00:00Z".to_string(),
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"now_iso\":\"2026-05-16T00:00:00Z\""));
        assert!(s.contains("acquire_build_slot"));
        assert!(s.contains("ignores expired leases"));
        assert!(!s.contains("acquire_build_slot --force"));
        assert!(s.contains("safe_fix_deferred"));
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
        let finding = IdentityBuildSlotLeaseExpiredFinding {
            entries: Vec::new(),
            now_iso: "2026-05-16T00:00:00Z".to_string(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
