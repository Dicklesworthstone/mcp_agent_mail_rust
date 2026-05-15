//! `fm-db-state-files-retained-autocommit-leak` — P0 detect-only.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! Frankensqlite's "retained autocommit" mode keeps write
//! transactions implicitly open until the connection is dropped
//! (or `COMMIT;` is explicitly issued). The Rust pool relies on
//! every connection running `PRAGMA autocommit_retain = OFF;`
//! at init so that durable visibility holds — otherwise writes
//! from one transaction stay invisible to readers on other
//! pooled connections until the writer's connection is recycled.
//!
//! The canonical regression test
//! `sqlite_pool_connection_disables_retained_autocommit_for_durable_visibility`
//! (`mcp-agent-mail-db::pool.rs`) asserts this PRAGMA must be in
//! `schema::build_conn_pragmas()`'s output. If a future refactor
//! drops the directive, durability silently degrades.
//!
//! ## Detection vector (NOT connection-local)
//!
//! Pass-35V was reverted because its detector queried
//! `PRAGMA busy_timeout` on a fresh connection — connection-local
//! PRAGMAs always read SQLite's default, regardless of the pool's
//! state. **This** detector avoids that trap by inspecting the
//! pool's compile-time init-SQL constant:
//!
//! - `mcp_agent_mail_db::schema::PRAGMA_CONN_SETTINGS_SQL` — the
//!   static template used to initialize every pooled connection.
//! - `mcp_agent_mail_db::schema::build_conn_pragmas()` — the
//!   dynamic generator that the pool actually calls.
//!
//! Both sources are inspected; if neither contains
//! `autocommit_retain = OFF`, the build is misconfigured and the
//! detector emits a P0 finding. The check is essentially a
//! runtime assertion that pool durability invariants survived
//! the build pipeline.
//!
//! ## Fix
//!
//! **Detect-only.** Mutating compile-time constants from the
//! doctor process is not feasible; the only correct remediation
//! is a source fix + rebuild + redeploy. Manual remediation
//! points operators at the relevant schema constant and asks
//! them to file an upstream bug.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

pub const FM_ID: &str = "fm-db-state-files-retained-autocommit-leak";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "db_state_files";

/// PRAGMA directive that must appear in the pool init SQL.
/// **Display value** — exact spelling used in finding evidence
/// and manual_remediation guidance. The detector accepts any
/// semantically equivalent form (case-insensitive, flexible
/// whitespace around `=`) per pass-35AA review F3 (Codex P2).
pub const REQUIRED_DIRECTIVE: &str = "autocommit_retain = OFF";

/// Case-insensitive regex matching the semantic invariant
/// `PRAGMA autocommit_retain = OFF` with flexible whitespace.
/// Avoids false positives when an upstream formatter rewrites
/// the PRAGMA without changing semantics (e.g.,
/// `PRAGMA autocommit_retain=OFF;` or `pragma autocommit_retain
/// = off`).
///
/// **Known limitation** (pass-35BB round-3 review F1, P3):
/// the regex does NOT recognize function-style SQLite PRAGMA
/// syntax `PRAGMA autocommit_retain(OFF)` or alternative
/// boolean literals (`0`, `false`, `no`). Our codebase only
/// emits the `name = value;` form, so the false-positive risk
/// is zero in practice. If a future schema refactor adopts
/// alternative spellings, this regex needs broadening to
/// `(?i)pragma\s+autocommit_retain\s*(?:=|\()\s*(?:off|0|false|no)\b\)?`.
fn directive_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)pragma\s+autocommit_retain\s*=\s*off\b")
            .expect("autocommit_retain regex must compile")
    })
}

/// Returns true if `sql` carries the required PRAGMA directive
/// in any semantically-equivalent form.
pub fn contains_required_directive(sql: &str) -> bool {
    directive_regex().is_match(sql)
}

#[derive(Debug, Clone, Serialize)]
pub struct RetainedAutocommitLeakFinding {
    /// Whether the static `PRAGMA_CONN_SETTINGS_SQL` constant
    /// contained the directive.
    pub static_const_ok: bool,
    /// Whether `build_conn_pragmas(...)` output contained the
    /// directive (probed with a realistic config).
    pub dynamic_output_ok: bool,
}

impl RetainedAutocommitLeakFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "pool init SQL is missing `{REQUIRED_DIRECTIVE}` (static_const_ok={}, dynamic_output_ok={}); durable visibility silently degraded",
            self.static_const_ok, self.dynamic_output_ok,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "required_directive": REQUIRED_DIRECTIVE,
                "static_const_ok": self.static_const_ok,
                "dynamic_output_ok": self.dynamic_output_ok,
                "manual_remediation": {
                    "steps": [
                        "Edit `crates/mcp-agent-mail-db/src/schema.rs`: ensure `PRAGMA_CONN_SETTINGS_SQL` contains the line `PRAGMA autocommit_retain = OFF;`.",
                        "Edit the same file's `build_conn_pragmas()` function so its formatted output also contains the directive.",
                        "Run the canonical regression test: `cargo test -p mcp-agent-mail-db pragma_busy_timeout_matches_legacy sqlite_pool_connection_disables_retained_autocommit_for_durable_visibility`.",
                        "Rebuild and redeploy `mcp-agent-mail` / `am` binaries.",
                    ],
                    "note": "Auto-fix is intentionally not implemented — the directive lives in compiled Rust code; only a rebuild can resolve the finding.",
                    "cross_ref_test": "sqlite_pool_connection_disables_retained_autocommit_for_durable_visibility (mcp-agent-mail-db/src/pool.rs)",
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

/// Inputs for the detector. Production callers leave both as
/// `None` (the detector then reads the compiled-in constants).
/// Tests can substitute fabricated SQL to exercise the matching
/// logic without depending on the actual pool config.
#[derive(Debug, Clone, Default)]
pub struct DetectInputs {
    pub static_pragma_override: Option<String>,
    pub dynamic_pragma_override: Option<String>,
}

/// Detector. PURE — reads compile-time constants from
/// `mcp_agent_mail_db::schema`.
pub fn detect(inputs: &DetectInputs) -> Vec<RetainedAutocommitLeakFinding> {
    let static_sql = inputs
        .static_pragma_override
        .clone()
        .unwrap_or_else(|| mcp_agent_mail_db::schema::PRAGMA_CONN_SETTINGS_SQL.to_string());
    // Probe build_conn_pragmas with a representative config
    // (4 connections, default cache budget). Both code paths
    // need to carry the directive.
    let dynamic_sql = inputs.dynamic_pragma_override.clone().unwrap_or_else(|| {
        mcp_agent_mail_db::schema::build_conn_pragmas(
            4,
            mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
        )
    });

    let static_ok = contains_required_directive(&static_sql);
    let dynamic_ok = contains_required_directive(&dynamic_sql);

    if static_ok && dynamic_ok {
        return Vec::new();
    }

    vec![RetainedAutocommitLeakFinding {
        static_const_ok: static_ok,
        dynamic_output_ok: dynamic_ok,
    }]
}

/// Fixer. Detect-only — manual rebuild required.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &RetainedAutocommitLeakFinding,
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

    /// **NEGATIVE TEST FIRST** (pass-35V lesson): production
    /// build (compile-time constants intact) → no finding.
    #[test]
    fn detector_skips_with_production_constants() {
        let inputs = DetectInputs::default();
        let findings = detect(&inputs);
        assert!(
            findings.is_empty(),
            "production pool init SQL must contain the directive; got finding(s)"
        );
    }

    #[test]
    fn detector_flags_when_static_const_missing_directive() {
        let inputs = DetectInputs {
            static_pragma_override: Some("PRAGMA foreign_keys = OFF;".to_string()),
            dynamic_pragma_override: None, // production dynamic is OK
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(!findings[0].static_const_ok);
        assert!(findings[0].dynamic_output_ok);
    }

    #[test]
    fn detector_flags_when_dynamic_output_missing_directive() {
        let inputs = DetectInputs {
            static_pragma_override: None, // production static is OK
            dynamic_pragma_override: Some("PRAGMA foreign_keys = OFF;".to_string()),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].static_const_ok);
        assert!(!findings[0].dynamic_output_ok);
    }

    #[test]
    fn detector_flags_when_both_sources_missing_directive() {
        let inputs = DetectInputs {
            static_pragma_override: Some("PRAGMA foreign_keys = OFF;".to_string()),
            dynamic_pragma_override: Some("PRAGMA foreign_keys = OFF;".to_string()),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(!findings[0].static_const_ok);
        assert!(!findings[0].dynamic_output_ok);
    }

    #[test]
    fn detector_accepts_directive_with_surrounding_whitespace() {
        // The check is substring-based; leading/trailing
        // whitespace or surrounding PRAGMA syntax doesn't matter.
        let inputs = DetectInputs {
            static_pragma_override: Some(
                "PRAGMA foo = 1;\nPRAGMA autocommit_retain = OFF;\nPRAGMA bar = 2;".to_string(),
            ),
            dynamic_pragma_override: Some(
                "PRAGMA autocommit_retain = OFF;\nPRAGMA wal_autocheckpoint = 1000;".to_string(),
            ),
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    /// pass-35AA review F3 (Codex P2): the matcher must accept
    /// semantically-equivalent forms — case-insensitive,
    /// flexible whitespace around `=`. Otherwise an innocuous
    /// formatter rewrite would start tripping a P0 doctor
    /// finding.
    #[test]
    fn matcher_accepts_semantic_equivalents() {
        // No whitespace around `=`.
        assert!(contains_required_directive("PRAGMA autocommit_retain=OFF;"));
        // Lowercase OFF.
        assert!(contains_required_directive(
            "PRAGMA autocommit_retain = off;"
        ));
        // Lowercase pragma.
        assert!(contains_required_directive(
            "pragma autocommit_retain = OFF;"
        ));
        // Extra whitespace.
        assert!(contains_required_directive(
            "PRAGMA  autocommit_retain  =   OFF;"
        ));
        // Tab as separator.
        assert!(contains_required_directive(
            "PRAGMA\tautocommit_retain\t=\tOFF;"
        ));
        // Mixed case OFF.
        assert!(contains_required_directive(
            "PRAGMA autocommit_retain = Off;"
        ));
    }

    #[test]
    fn matcher_rejects_off_in_unrelated_pragma() {
        // The `\b` word-boundary anchor on `off` prevents
        // matching `off_with_my_head` etc.
        assert!(!contains_required_directive(
            "PRAGMA autocommit_retain = off_some_extension;"
        ));
        // Not the autocommit_retain pragma.
        assert!(!contains_required_directive("PRAGMA something_else = OFF;"));
        // Different pragma name with the substring.
        assert!(!contains_required_directive(
            "PRAGMA autocommit_retain_disabled = OFF;"
        ));
    }

    #[test]
    fn finding_serializes_with_both_source_flags() {
        let f = RetainedAutocommitLeakFinding {
            static_const_ok: false,
            dynamic_output_ok: true,
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"static_const_ok\":false"));
        assert!(s.contains("\"dynamic_output_ok\":true"));
        assert!(s.contains("autocommit_retain = OFF"));
        assert!(s.contains("\"auto_fixable\":false"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), "test_run").unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = crate::doctor::mutate::MutateContext {
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
        let finding = RetainedAutocommitLeakFinding {
            static_const_ok: false,
            dynamic_output_ok: false,
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
