//! `fm-identity_contacts_state-build-slot-lease-expired` — P2
//! auto-fix via `Op::WriteFile`.
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
//! **Auto-fix via `Op::WriteFile` (UPDATE-only — never delete
//! per RULE 1):** for each expired-lease entry, re-read the
//! lease JSON, set `released_ts` to the finding's `now_iso`
//! (so the operator's evidence and the on-disk record agree),
//! and rewrite the file through the chokepoint. All sibling
//! JSON fields are preserved verbatim — `serde_json` is built
//! with `preserve_order` workspace-wide, so the new bytes differ
//! from the old only at the `released_ts` value. The chokepoint
//! backs up the original bytes verbatim, so `am doctor undo
//! <run-id>` restores byte-identical leases (including
//! `released_ts: null` or the absent-field shape).
//!
//! Mode preservation: the chosen `Op::WriteFile` mode mirrors
//! the live file's mode at fix-time so the visible permissions
//! don't shift under operators. (Undo restores to `before_mode`
//! anyway via the backup, so this is a quality-of-life choice,
//! not a correctness one.)
//!
//! Entries that vanish between detect-time and fix-time count
//! as `actions_skipped`. Entries that the operator (or another
//! agent / sibling FM) already released between detect and fix
//! — i.e. `released_ts` is now non-null — also count as
//! `actions_skipped` so the fix is idempotent.
//!
//! ## Concurrency note (bounded TOCTOU)
//!
//! fix() reads the lease content, then calls `mutate()`. Between
//! those two steps a concurrent server-side `release_build_slot`
//! call could overwrite the lease. The chokepoint's
//! `TamperedBeforeMutate` check only fires for changes WITHIN the
//! chokepoint's hash-then-backup window — not for changes BEFORE
//! the chokepoint sees the file. Result: a concurrent fresh
//! release between our read and our write would be silently
//! overwritten by our `released_ts = now_iso` rewrite.
//!
//! This is acceptable because:
//! - The doctor's premise is operator awareness; running
//!   `am doctor fix` while the system is live is operator error.
//! - The cost of overwriting a fresh release is minimal: the
//!   lease still records a `released_ts` (just `now_iso` instead
//!   of the writer's chosen ts).
//! - The race window is microseconds (read → parse → mutate).
//!
//! A future hardening pass could re-read the live file inside
//! the chokepoint right before write, but that requires extending
//! the `mutate()` API surface (currently overwrite-only).

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError, Op, mutate};
use serde::Serialize;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-identity_contacts_state-build-slot-lease-expired";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "identity_contacts_state";

/// Fallback mode for `Op::WriteFile` when the live lease file's
/// own mode can't be read (e.g., a vanished-but-checked race). In
/// practice fix() will never reach this constant because vanished
/// paths return early via `actions_skipped`.
const FALLBACK_LEASE_MODE: u32 = 0o644;

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
                "auto_fix_summary": format!(
                    "`am doctor fix --only {FM_ID} --yes` rewrites each lease's `released_ts` to `{}` via Op::WriteFile through the chokepoint, preserving every other JSON field. UPDATE-only: never deletes the lease file (per RULE 1). Reversible via `am doctor undo <run-id>` — the chokepoint backs up the original bytes verbatim.",
                    self.now_iso,
                ),
                "manual_remediation": {
                    "steps": [
                        "Auto-fix (preferred): `am doctor fix --only fm-identity_contacts_state-build-slot-lease-expired --yes`. The chokepoint rewrites each expired lease's `released_ts` to `now_iso` and records the original bytes so `am doctor undo <run-id>` is byte-identical-reversible.",
                        "Before invoking auto-fix: confirm the holder is actually dead. If the holder is alive but past `expires_ts`, the right answer is `renew_build_slot`, NOT a `released_ts` rewrite. The auto-fix assumes the holder is gone (which is true for the cases this FM catches: crash, OOM, network partition, SIGTERM mid-lifecycle).",
                        "Manual alternative: call `acquire_build_slot` for the same project, agent, and slot. Expired leases are ignored by conflict detection and the caller's lease path is rewritten.",
                        "Per-lease alternative: `am robot reservations --conflicts --expiring 30` shows the same ghost-lease state via the canonical robot CLI; pair with `force_release_file_reservation` for slots that map to a file reservation.",
                        "Re-run `am doctor fix --only fm-identity_contacts_state-build-slot-lease-expired --list` to confirm zero residual ghost leases.",
                    ],
                    "warning": "Do NOT delete lease files manually — the chokepoint and the build-slot API rely on lease files as the canonical record. Use the auto-fix, the manual API alternatives above, or `acquire_build_slot`/`renew_build_slot`/`release_build_slot` so lease state is written through the API.",
                    "common_causes": [
                        "Agent crashed / OOMed / network-partitioned without calling `release_build_slot`.",
                        "`am serve` SIGTERM'd mid-build-slot lifecycle (the cleanup_pane_identities pass may not have run).",
                        "Manual `kill` of an agent holding a long lease.",
                        "Network outage during release request (the agent thinks it released; the server never got the call).",
                    ],
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor fix --only {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: true,
                estimated_actions: self.entries.len(),
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

/// Fixer. Routes through `mutate()` with `Op::WriteFile` per
/// expired-lease entry.
///
/// For each entry:
/// 1. Skip if the lease file has vanished since detect-time
///    (`actions_skipped += 1`).
/// 2. Re-read the lease JSON. If parsing fails, skip (the file
///    was tampered with between detect and fix; safer to not
///    rewrite than to risk corrupting the operator's evidence).
/// 3. If `released_ts` is already non-null (a sibling agent or
///    a manual `release_build_slot` resolved it between probes),
///    skip — the FM is already moot for this entry.
/// 4. Set `released_ts` to `finding.now_iso` (preserves the
///    consistency: the operator's evidence and the on-disk
///    state agree on when the doctor recorded the release).
/// 5. Serialize back to JSON. `preserve_order` is enabled
///    workspace-wide so sibling field ordering is preserved.
/// 6. Op::WriteFile the new bytes with the live file's existing
///    mode (no permission-bit surprise for the operator).
///
/// All chokepoint guarantees apply: verbatim backup, hash witness,
/// atomic write-tmp-then-rename, advisory lock, undo restores
/// byte-identical originals.
pub fn fix(
    ctx: &MutateContext,
    finding: &IdentityBuildSlotLeaseExpiredFinding,
) -> Result<FixOutcome, MutateError> {
    let mut actions_taken = 0;
    let mut actions_skipped = 0;
    for entry in &finding.entries {
        let body = match std::fs::read_to_string(&entry.lease_path) {
            Ok(b) => b,
            Err(_) => {
                actions_skipped += 1;
                continue;
            }
        };
        let mut value: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                actions_skipped += 1;
                continue;
            }
        };
        let already_released = value.get("released_ts").is_some_and(|r| !r.is_null());
        if already_released {
            actions_skipped += 1;
            continue;
        }
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "released_ts".to_string(),
                serde_json::Value::String(finding.now_iso.clone()),
            );
        } else {
            // Top-level wasn't a JSON object (e.g., array or
            // scalar). The detector only emits findings against
            // object-shaped leases (it reads `released_ts` /
            // `expires_ts` via `.get(...)`), so this branch is
            // unreachable in practice. Skip defensively.
            actions_skipped += 1;
            continue;
        }
        // Match `mcp_agent_mail_tools::build_slots::write_lease_json`
        // exactly: `to_string_pretty` with NO trailing newline.
        // Any byte difference vs the server's writer would surface
        // as operator-visible churn in diff tools.
        let new_body = match serde_json::to_string_pretty(&value) {
            Ok(s) => s,
            Err(_) => {
                actions_skipped += 1;
                continue;
            }
        };
        // Mirror the live file's mode so fix() doesn't surprise-
        // change permissions. If we can't read mode (race), fall
        // back to a sensible default — though step 1 already
        // guards the vanished case.
        let mode = std::fs::symlink_metadata(&entry.lease_path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(FALLBACK_LEASE_MODE);
        mutate(
            ctx,
            &entry.lease_path,
            Op::WriteFile {
                content: new_body.into_bytes(),
                mode,
            },
        )?;
        actions_taken += 1;
    }
    Ok(FixOutcome {
        actions_taken,
        actions_skipped,
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
        assert!(!s.contains("acquire_build_slot --force"));
        assert!(s.contains("auto_fix_summary"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":true"));
        assert!(s.contains("\"estimated_actions\":1"));
    }

    fn ctx_for(td: &TempDir, run_id: &str) -> MutateContext {
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        MutateContext {
            run_id: run_id.into(),
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
        }
    }

    /// **NEGATIVE TEST FIRST**: empty entries → no actions, no
    /// skips. A degenerate baseline.
    #[test]
    fn fixer_with_empty_entries_is_a_no_op() {
        let td = TempDir::new().unwrap();
        let ctx = ctx_for(&td, "2026-05-16T00-00-00Z__lease_empty");
        let finding = IdentityBuildSlotLeaseExpiredFinding {
            entries: Vec::new(),
            now_iso: "2026-05-16T00:00:00Z".to_string(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 0);
    }

    /// **NEGATIVE**: an entry whose lease file vanished between
    /// detect and fix is skipped, never errors.
    #[test]
    fn fixer_skips_vanished_lease() {
        let td = TempDir::new().unwrap();
        let ctx = ctx_for(&td, "2026-05-16T00-00-00Z__lease_vanished");
        let finding = IdentityBuildSlotLeaseExpiredFinding {
            entries: vec![ExpiredLeaseEntry {
                lease_path: td.path().join("ghost-lease.json"),
                project_slug: "demo".to_string(),
                slot_name: "build-1".to_string(),
                acquired_ts: None,
                expires_ts: "2020-01-01T00:00:00Z".to_string(),
                holder: None,
            }],
            now_iso: "2026-05-16T00:00:00Z".to_string(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    /// **NEGATIVE**: a lease that's already non-null
    /// `released_ts` (sibling resolution race) is skipped — the
    /// FM is moot for that entry.
    #[test]
    fn fixer_skips_already_released_lease() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        let lease = write_lease(
            &root,
            "demo",
            "build-1",
            "lease",
            r#"{"expires_ts":"2020-01-01T00:00:00Z","released_ts":"2020-01-02T00:00:00Z"}"#,
        );
        let ctx = ctx_for(&td, "2026-05-16T00-00-00Z__lease_already");
        let finding = IdentityBuildSlotLeaseExpiredFinding {
            entries: vec![ExpiredLeaseEntry {
                lease_path: lease.clone(),
                project_slug: "demo".to_string(),
                slot_name: "build-1".to_string(),
                acquired_ts: None,
                expires_ts: "2020-01-01T00:00:00Z".to_string(),
                holder: None,
            }],
            now_iso: "2026-05-16T00:00:00Z".to_string(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        // File content untouched.
        let post = fs::read_to_string(&lease).unwrap();
        assert!(post.contains("2020-01-02T00:00:00Z"));
    }

    #[test]
    fn fixer_writes_released_ts_to_now_iso_preserving_siblings() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        // Sibling fields (`acquired_ts`, `holder`, custom keys)
        // must survive the rewrite verbatim.
        let original = r#"{"expires_ts":"2020-01-01T00:00:00Z","released_ts":null,"acquired_ts":"2019-12-31T00:00:00Z","holder":"GhostHolder","slot_metadata":{"label":"build-alpha","priority":7}}"#;
        let lease = write_lease(&root, "demo", "build-1", "GhostHolder", original);
        let ctx = ctx_for(&td, "2026-05-16T00-00-00Z__lease_fix");
        let now_iso = "2026-05-16T12:34:56Z".to_string();
        let finding = IdentityBuildSlotLeaseExpiredFinding {
            entries: vec![ExpiredLeaseEntry {
                lease_path: lease.clone(),
                project_slug: "demo".to_string(),
                slot_name: "build-1".to_string(),
                acquired_ts: Some("2019-12-31T00:00:00Z".to_string()),
                expires_ts: "2020-01-01T00:00:00Z".to_string(),
                holder: Some("GhostHolder".to_string()),
            }],
            now_iso: now_iso.clone(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);

        let post = fs::read_to_string(&lease).unwrap();
        let post_value: serde_json::Value = serde_json::from_str(&post).unwrap();
        assert_eq!(
            post_value.get("released_ts").and_then(|v| v.as_str()),
            Some(now_iso.as_str()),
            "released_ts must be set to finding.now_iso"
        );
        // Sibling fields preserved.
        assert_eq!(
            post_value.get("expires_ts").and_then(|v| v.as_str()),
            Some("2020-01-01T00:00:00Z")
        );
        assert_eq!(
            post_value.get("acquired_ts").and_then(|v| v.as_str()),
            Some("2019-12-31T00:00:00Z")
        );
        assert_eq!(
            post_value.get("holder").and_then(|v| v.as_str()),
            Some("GhostHolder")
        );
        let nested = post_value.get("slot_metadata").unwrap();
        assert_eq!(
            nested.get("label").and_then(|v| v.as_str()),
            Some("build-alpha")
        );
        assert_eq!(nested.get("priority").and_then(|v| v.as_u64()), Some(7));
    }

    /// Tampered / non-JSON lease file is skipped (the chokepoint
    /// is never invoked on data we can't parse).
    #[test]
    fn fixer_skips_malformed_lease_json() {
        let td = TempDir::new().unwrap();
        let root = make_storage_root(&td);
        let lease = write_lease(&root, "demo", "build-1", "lease", "not json {{");
        let ctx = ctx_for(&td, "2026-05-16T00-00-00Z__lease_malformed");
        let finding = IdentityBuildSlotLeaseExpiredFinding {
            entries: vec![ExpiredLeaseEntry {
                lease_path: lease.clone(),
                project_slug: "demo".to_string(),
                slot_name: "build-1".to_string(),
                acquired_ts: None,
                expires_ts: "2020-01-01T00:00:00Z".to_string(),
                holder: None,
            }],
            now_iso: "2026-05-16T00:00:00Z".to_string(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        let post = fs::read_to_string(&lease).unwrap();
        assert_eq!(post, "not json {{");
    }
}
