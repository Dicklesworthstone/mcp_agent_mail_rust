//! `fm-archive-state-files-missing-or-malformed-project-json` —
//! P1 partial auto-fix via `Op::WriteFile`.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! Every `<storage_root>/projects/<slug>/` directory must
//! contain a `project.json` file with valid JSON and the required
//! `slug` + `human_key` fields. Missing or malformed metadata
//! breaks many downstream paths: the TUI's projects screen
//! shows blank entries; `am robot status` returns errors;
//! archive replay can't reconstruct the project identity.
//!
//! ## Detection
//!
//! Wraps `mcp_agent_mail_db::archive_anomaly::scan_archive_anomalies(...)`
//! and filters its report for `MissingProjectMetadata` OR
//! `InvalidProjectMetadata`. Mirrors the FM5 pattern (also a
//! `scan_archive_anomalies` wrapper) — see
//! `suspicious_ephemeral_archive_root.rs` for the same shape.
//!
//! ## Fix (partial auto-fix)
//!
//! Auto-fix scope is bounded by what the DB-aware scan can
//! supply for free:
//!
//! - **Invalid + canonical_human_key=Some**: the malformed
//!   `project.json` is rewritten via `Op::WriteFile` to the
//!   canonical shape
//!   `{"slug": "<slug>", "human_key": "<canonical_human_key>"}`
//!   (matching `storage::write_project_metadata_with_config`'s
//!   serialization: `serde_json::to_string_pretty`, no trailing
//!   newline, mode 0o644). The chokepoint backs up the original
//!   malformed bytes verbatim, so `am doctor undo <run-id>`
//!   restores the pre-fix state byte-identically.
//! - **Missing**: skipped. The detector only knows
//!   `fallback_slug` (the directory basename); a valid
//!   `project.json` also requires `human_key`, which is an
//!   absolute filesystem path the operator must supply. Even
//!   though the underlying anomaly library classifies this as
//!   `SafeAuto`, the canonical writer
//!   (`storage::write_project_metadata_with_config:4469`)
//!   rejects relative / empty `human_key`, so a slug-only file
//!   would error downstream. Stays manual.
//! - **Invalid + canonical_human_key=None**: skipped. Same
//!   reason — no source of truth for `human_key`. Stays manual.
//!
//! `dispatch_only` and `detect_only` both route through
//! `db_aware_archive_report(inputs)` (pass-35CO), so production
//! callers with a configured DB get `canonical_human_key`
//! populated for free.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError, Op, mutate};
use mcp_agent_mail_db::archive_anomaly::{
    ArchiveAnomalyKind, ArchiveAnomalyReport, scan_archive_anomalies,
};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-missing-or-malformed-project-json";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "archive_state_files";

/// Canonical mode for `project.json` (matches
/// `storage::archive_append_open_options` which uses 0o644 for
/// archive append targets).
const PROJECT_JSON_MODE: u32 = 0o644;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectJsonProblem {
    /// `project.json` does not exist in the project directory.
    Missing {
        project_dir: PathBuf,
        fallback_slug: String,
    },
    /// `project.json` exists but is invalid JSON or missing
    /// required fields.
    Invalid {
        path: PathBuf,
        slug: String,
        canonical_human_key: Option<String>,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingOrMalformedProjectJsonFinding {
    pub problems: Vec<ProjectJsonProblem>,
}

/// Auto-fix scope: only `Invalid` problems with
/// `canonical_human_key: Some(_)` are reconstructable from
/// available information. See module docstring for the full
/// classification.
fn is_auto_fixable(problem: &ProjectJsonProblem) -> bool {
    matches!(
        problem,
        ProjectJsonProblem::Invalid {
            canonical_human_key: Some(_),
            ..
        }
    )
}

/// Number of problems in a finding that fix() would attempt to
/// rewrite. Used to populate `FindingRemediation.estimated_actions`
/// + decide `auto_fixable` per-finding.
fn count_fixable(problems: &[ProjectJsonProblem]) -> usize {
    problems.iter().filter(|p| is_auto_fixable(p)).count()
}

impl MissingOrMalformedProjectJsonFinding {
    pub fn to_finding(&self) -> super::Finding {
        let n_missing = self
            .problems
            .iter()
            .filter(|p| matches!(p, ProjectJsonProblem::Missing { .. }))
            .count();
        let n_invalid = self.problems.len() - n_missing;
        let fixable = count_fixable(&self.problems);
        let unfixable = self.problems.len() - fixable;
        let title = format!(
            "{} project(s) in archive have missing or malformed `project.json` ({} missing, {} invalid; {} auto-fixable, {} need operator input)",
            self.problems.len(),
            n_missing,
            n_invalid,
            fixable,
            unfixable,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.95,
            evidence: serde_json::json!({
                "problems": self.problems,
                "missing_count": n_missing,
                "invalid_count": n_invalid,
                "fixable_count": fixable,
                "unfixable_count": unfixable,
                "auto_fix_summary": format!(
                    "`am doctor fix --only {FM_ID} --yes` rewrites {fixable} Invalid project.json file(s) where the DB-aware scan supplied `canonical_human_key`. The remaining {unfixable} (Missing entries + Invalid entries without canonical_human_key) stay in `actions_skipped` and need operator-supplied `human_key`. Reversible via `am doctor undo <run-id>` — the chokepoint backs up the original malformed bytes verbatim."
                ),
                "manual_remediation": {
                    "steps": [
                        "Auto-fix (where applicable): `am doctor fix --only fm-archive-state-files-missing-or-malformed-project-json --yes`. Rewrites every Invalid entry whose `canonical_human_key` is known (from the DB-aware scan). Missing entries + Invalid-without-canonical entries are skipped — they need operator-supplied truth.",
                        "For each Missing entry: inspect `project_dir` to identify the intended project. Create a minimal `project.json` with `slug` (the directory's basename) and `human_key` (the project's canonical filesystem path — must be absolute).",
                        "For each Invalid entry without canonical_human_key: read the `detail` field. Either configure the DB so the scan can populate canonical_human_key (then re-run the auto-fix), or edit `path` manually to fix the JSON / add the missing required field.",
                        "Re-run `am doctor fix --only fm-archive-state-files-missing-or-malformed-project-json --list` to confirm the anomaly is gone.",
                    ],
                    "note": "Auto-fix produces `{\"slug\": \"<slug>\", \"human_key\": \"<canonical_human_key>\"}` matching `storage::write_project_metadata_with_config`'s serialization (to_string_pretty, no trailing newline, mode 0o644).",
                },
            }),
            remediation: FindingRemediation {
                command: if fixable > 0 {
                    format!("am doctor fix --only {FM_ID}")
                } else {
                    format!("am doctor explain {FM_ID}")
                },
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: fixable > 0,
                estimated_actions: fixable,
            },
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DetectInputs {
    pub storage_root_override: Option<PathBuf>,
    pub report_override: Option<ArchiveAnomalyReport>,
}

pub fn detect(inputs: &DetectInputs) -> Vec<MissingOrMalformedProjectJsonFinding> {
    let report = match inputs.report_override.clone() {
        Some(r) => r,
        None => {
            let Some(root) = inputs.storage_root_override.clone() else {
                return Vec::new();
            };
            if !root.is_dir() {
                return Vec::new();
            }
            scan_archive_anomalies(&root)
        }
    };
    let problems: Vec<ProjectJsonProblem> = report
        .anomalies
        .iter()
        .filter_map(|a| match &a.kind {
            ArchiveAnomalyKind::MissingProjectMetadata {
                project_dir,
                fallback_slug,
            } => Some(ProjectJsonProblem::Missing {
                project_dir: project_dir.clone(),
                fallback_slug: fallback_slug.clone(),
            }),
            ArchiveAnomalyKind::InvalidProjectMetadata {
                path,
                slug,
                canonical_human_key,
                detail,
            } => Some(ProjectJsonProblem::Invalid {
                path: path.clone(),
                slug: slug.clone(),
                canonical_human_key: canonical_human_key.clone(),
                detail: detail.clone(),
            }),
            _ => None,
        })
        .collect();
    if problems.is_empty() {
        return Vec::new();
    }
    vec![MissingOrMalformedProjectJsonFinding { problems }]
}

/// Fixer. Iterates the finding's problems; for each
/// `Invalid + canonical_human_key=Some` entry, routes through
/// `mutate()` with `Op::WriteFile` carrying the canonical
/// reconstructed JSON. Missing entries and Invalid entries
/// without canonical_human_key are counted as `actions_skipped`
/// (they require operator-supplied truth and stay manual per
/// the module docstring).
pub fn fix(
    ctx: &MutateContext,
    finding: &MissingOrMalformedProjectJsonFinding,
) -> Result<FixOutcome, MutateError> {
    let mut actions_taken = 0;
    let mut actions_skipped = 0;
    for problem in &finding.problems {
        let ProjectJsonProblem::Invalid {
            path,
            slug,
            canonical_human_key: Some(human_key),
            ..
        } = problem
        else {
            actions_skipped += 1;
            continue;
        };
        // Defense-in-depth: the canonical writer
        // (`storage::write_project_metadata_with_config:4469`)
        // rejects non-absolute human_key. Skip rather than risk
        // writing a project.json that downstream code can't read.
        if !std::path::Path::new(human_key).is_absolute() {
            actions_skipped += 1;
            continue;
        }
        let canonical = serde_json::json!({
            "slug": slug,
            "human_key": human_key,
        });
        // Match `storage::write_json`: `to_string_pretty` with
        // no trailing newline. Any byte difference vs the
        // server's writer would surface as diff churn.
        let new_body = match serde_json::to_string_pretty(&canonical) {
            Ok(s) => s,
            Err(_) => {
                actions_skipped += 1;
                continue;
            }
        };
        mutate(
            ctx,
            path,
            Op::WriteFile {
                content: new_body.into_bytes(),
                mode: PROJECT_JSON_MODE,
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
    use mcp_agent_mail_db::archive_anomaly::ArchiveAnomaly;

    /// **NEGATIVE TEST FIRST** (pass-35V lesson): empty report
    /// → no finding.
    #[test]
    fn detector_skips_clean_report() {
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(ArchiveAnomalyReport::new()),
        };
        let findings = detect(&inputs);
        assert!(
            findings.is_empty(),
            "empty anomaly report must not emit a finding"
        );
    }

    /// **NEGATIVE TEST**: report has unrelated anomalies (e.g.,
    /// SuspiciousEphemeralProject — that's FM5's domain) → no
    /// finding from this FM.
    #[test]
    fn detector_skips_report_with_unrelated_anomalies() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::SuspiciousEphemeralProject {
                project_dir: "/tmp/x".into(),
                slug: "x".to_string(),
                human_key: Some("/tmp/x".to_string()),
                reason: "tmp-rooted".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert!(
            findings.is_empty(),
            "SuspiciousEphemeralProject is FM5's domain; must not surface here"
        );
    }

    #[test]
    fn detector_skips_when_no_inputs() {
        let inputs = DetectInputs::default();
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_nonexistent_storage_root() {
        let inputs = DetectInputs {
            storage_root_override: Some("/nonexistent/path".into()),
            report_override: None,
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_flags_missing_project_metadata() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingProjectMetadata {
                project_dir: "/var/data/projects/foo".into(),
                fallback_slug: "foo".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].problems.len(), 1);
        assert!(matches!(
            &findings[0].problems[0],
            ProjectJsonProblem::Missing { fallback_slug, .. } if fallback_slug == "foo"
        ));
    }

    #[test]
    fn detector_flags_invalid_project_metadata() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::InvalidProjectMetadata {
                path: "/var/data/projects/foo/project.json".into(),
                slug: "foo".to_string(),
                canonical_human_key: None,
                detail: "malformed JSON: expected value at line 1 column 1".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].problems.len(), 1);
        assert!(matches!(
            &findings[0].problems[0],
            ProjectJsonProblem::Invalid { detail, .. } if detail.contains("malformed JSON")
        ));
    }

    #[test]
    fn detector_aggregates_mixed_problems_into_one_finding() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingProjectMetadata {
                project_dir: "/p/a".into(),
                fallback_slug: "a".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::InvalidProjectMetadata {
                path: "/p/b/project.json".into(),
                slug: "b".to_string(),
                canonical_human_key: Some("/work/b".to_string()),
                detail: "missing slug field".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingProjectMetadata {
                project_dir: "/p/c".into(),
                fallback_slug: "c".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].problems.len(), 3);
    }

    #[test]
    fn finding_serializes_with_problem_breakdown_when_no_canonical() {
        // No problem has canonical_human_key → entire finding is
        // unfixable; auto_fixable: false.
        let f = MissingOrMalformedProjectJsonFinding {
            problems: vec![
                ProjectJsonProblem::Missing {
                    project_dir: "/p/a".into(),
                    fallback_slug: "a".to_string(),
                },
                ProjectJsonProblem::Invalid {
                    path: "/p/b/project.json".into(),
                    slug: "b".to_string(),
                    canonical_human_key: None,
                    detail: "bad json".to_string(),
                },
            ],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"missing_count\":1"));
        assert!(s.contains("\"invalid_count\":1"));
        assert!(s.contains("\"fixable_count\":0"));
        assert!(s.contains("\"unfixable_count\":2"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("manual_remediation"));
    }

    #[test]
    fn finding_serializes_as_auto_fixable_when_canonical_present() {
        // At least one Invalid problem has canonical_human_key →
        // finding flips to auto_fixable: true with
        // estimated_actions equal to the fixable count.
        let f = MissingOrMalformedProjectJsonFinding {
            problems: vec![
                ProjectJsonProblem::Missing {
                    project_dir: "/p/a".into(),
                    fallback_slug: "a".to_string(),
                },
                ProjectJsonProblem::Invalid {
                    path: "/p/b/project.json".into(),
                    slug: "b".to_string(),
                    canonical_human_key: Some("/work/b".to_string()),
                    detail: "missing slug field".to_string(),
                },
                ProjectJsonProblem::Invalid {
                    path: "/p/c/project.json".into(),
                    slug: "c".to_string(),
                    canonical_human_key: Some("/work/c".to_string()),
                    detail: "trailing comma".to_string(),
                },
            ],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("\"fixable_count\":2"));
        assert!(s.contains("\"unfixable_count\":1"));
        assert!(s.contains("\"auto_fixable\":true"));
        assert!(s.contains("\"estimated_actions\":2"));
        assert!(s.contains("auto_fix_summary"));
    }

    fn ctx_for(td: &tempfile::TempDir, run_id: &str) -> MutateContext {
        use std::fs;
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

    /// **NEGATIVE TEST FIRST**: empty problems → no-op (no
    /// actions, no skips). Degenerate baseline.
    #[test]
    fn fixer_with_empty_problems_is_a_no_op() {
        let td = tempfile::TempDir::new().unwrap();
        let ctx = ctx_for(&td, "2026-05-16T00-00-00Z__proj_empty");
        let finding = MissingOrMalformedProjectJsonFinding { problems: vec![] };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 0);
    }

    /// **NEGATIVE**: only Missing entries → all skipped (no
    /// canonical_human_key available, operator-supplied truth
    /// required).
    #[test]
    fn fixer_skips_missing_entries() {
        let td = tempfile::TempDir::new().unwrap();
        let ctx = ctx_for(&td, "2026-05-16T00-00-00Z__proj_missing");
        let finding = MissingOrMalformedProjectJsonFinding {
            problems: vec![ProjectJsonProblem::Missing {
                project_dir: "/p/a".into(),
                fallback_slug: "a".to_string(),
            }],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    /// **NEGATIVE**: Invalid without canonical_human_key → skipped
    /// (no source of truth).
    #[test]
    fn fixer_skips_invalid_without_canonical() {
        let td = tempfile::TempDir::new().unwrap();
        let ctx = ctx_for(&td, "2026-05-16T00-00-00Z__proj_invalid_no_canon");
        let finding = MissingOrMalformedProjectJsonFinding {
            problems: vec![ProjectJsonProblem::Invalid {
                path: td.path().join("project.json"),
                slug: "x".to_string(),
                canonical_human_key: None,
                detail: "bad json".to_string(),
            }],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    /// **NEGATIVE**: Invalid with empty-string canonical_human_key
    /// → skipped. `Path::new("").is_absolute()` is `false`, so
    /// the non-absolute defense-in-depth path catches this. Pin
    /// the behavior so a future refactor of the check can't
    /// regress into accepting empty `human_key` and writing a
    /// `{"slug": "...", "human_key": ""}` JSON that the canonical
    /// writer would have rejected.
    #[test]
    fn fixer_skips_invalid_with_empty_canonical_path() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let pj = td.path().join("project.json");
        fs::write(&pj, "not json").unwrap();
        let ctx = ctx_for(&td, "2026-05-18T00-00-00Z__proj_empty_canon");
        let finding = MissingOrMalformedProjectJsonFinding {
            problems: vec![ProjectJsonProblem::Invalid {
                path: pj.clone(),
                slug: "x".to_string(),
                canonical_human_key: Some(String::new()),
                detail: "bad json".to_string(),
            }],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        // File untouched.
        let post = fs::read_to_string(&pj).unwrap();
        assert_eq!(post, "not json");
    }

    /// **NEGATIVE**: Invalid with non-absolute canonical_human_key
    /// → skipped (canonical writer rejects non-absolute paths).
    #[test]
    fn fixer_skips_invalid_with_relative_canonical_path() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let pj = td.path().join("project.json");
        fs::write(&pj, "not json").unwrap();
        let ctx = ctx_for(&td, "2026-05-16T00-00-00Z__proj_relative_canon");
        let finding = MissingOrMalformedProjectJsonFinding {
            problems: vec![ProjectJsonProblem::Invalid {
                path: pj.clone(),
                slug: "x".to_string(),
                canonical_human_key: Some("relative/path".to_string()),
                detail: "bad json".to_string(),
            }],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        // File untouched.
        let post = fs::read_to_string(&pj).unwrap();
        assert_eq!(post, "not json");
    }

    /// Positive: Invalid with absolute canonical_human_key →
    /// rewrite via Op::WriteFile, sibling fields go away (this
    /// is a CLEAN reconstruction; the canonical project.json
    /// shape is `{slug, human_key}` only).
    #[test]
    fn fixer_rewrites_invalid_with_canonical_path() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let pj = td.path().join("project.json");
        // Plant a malformed project.json
        fs::write(&pj, r#"{"slug": "demo", "bad-key": "x"#).unwrap();
        let ctx = ctx_for(&td, "2026-05-16T00-00-00Z__proj_invalid_canon");
        let finding = MissingOrMalformedProjectJsonFinding {
            problems: vec![ProjectJsonProblem::Invalid {
                path: pj.clone(),
                slug: "demo".to_string(),
                canonical_human_key: Some("/workspaces/demo".to_string()),
                detail: "unterminated string".to_string(),
            }],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);
        let post = fs::read_to_string(&pj).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&post).unwrap();
        assert_eq!(parsed.get("slug").and_then(|v| v.as_str()), Some("demo"));
        assert_eq!(
            parsed.get("human_key").and_then(|v| v.as_str()),
            Some("/workspaces/demo")
        );
    }

    /// Mixed problems: only Invalid-with-canonical entries get
    /// rewritten; Missing and Invalid-without-canonical count as
    /// skipped.
    #[test]
    fn fixer_handles_mixed_problems() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let fixable = td.path().join("fixable_project.json");
        let unfixable = td.path().join("unfixable_project.json");
        fs::write(&fixable, "bad").unwrap();
        fs::write(&unfixable, "bad").unwrap();
        let ctx = ctx_for(&td, "2026-05-16T00-00-00Z__proj_mixed");
        let finding = MissingOrMalformedProjectJsonFinding {
            problems: vec![
                ProjectJsonProblem::Missing {
                    project_dir: td.path().join("p1"),
                    fallback_slug: "p1".to_string(),
                },
                ProjectJsonProblem::Invalid {
                    path: fixable.clone(),
                    slug: "demo".to_string(),
                    canonical_human_key: Some("/abs/demo".to_string()),
                    detail: "x".to_string(),
                },
                ProjectJsonProblem::Invalid {
                    path: unfixable.clone(),
                    slug: "demo2".to_string(),
                    canonical_human_key: None,
                    detail: "x".to_string(),
                },
            ],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 2);
        // Fixable file rewritten.
        let post_fixable: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&fixable).unwrap()).unwrap();
        assert_eq!(
            post_fixable.get("human_key").and_then(|v| v.as_str()),
            Some("/abs/demo")
        );
        // Unfixable file untouched.
        assert_eq!(fs::read_to_string(&unfixable).unwrap(), "bad");
    }
}
