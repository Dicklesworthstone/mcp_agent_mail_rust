//! Deployment validation, diagnostics, and reporting for static exports.
//!
//! Provides pre-flight checks, deployment report generation, platform-specific
//! configuration helpers, rollback guidance, post-deploy verification, and
//! security expectation documentation for GitHub Pages, Cloudflare Pages,
//! Netlify, and S3.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ShareError, ShareResult};

// ── Deployment report ───────────────────────────────────────────────────

/// Machine-readable deployment report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployReport {
    /// When this report was generated.
    pub generated_at: String,
    /// Overall deployment readiness.
    pub ready: bool,
    /// Pre-flight check results.
    pub checks: Vec<DeployCheck>,
    /// Detected hosting platforms.
    pub platforms: Vec<PlatformInfo>,
    /// Bundle statistics.
    pub bundle_stats: BundleStats,
    /// File integrity checksums.
    pub integrity: BTreeMap<String, String>,
    /// Security expectations for the deployment.
    pub security: SecurityExpectations,
    /// Rollback guidance for the deployment.
    pub rollback: RollbackGuidance,
}

/// A single pre-flight check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployCheck {
    pub name: String,
    pub passed: bool,
    pub message: String,
    pub severity: CheckSeverity,
}

/// Check severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckSeverity {
    Error,
    Warning,
    Info,
    /// Check precondition not met — not evaluated.
    Skipped,
}

/// Platform deployment information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformInfo {
    pub id: String,
    pub name: String,
    pub detected: bool,
    pub config_present: bool,
    pub deploy_command: Option<String>,
}

/// Bundle file statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleStats {
    pub total_files: usize,
    pub total_bytes: u64,
    pub html_pages: usize,
    pub data_files: usize,
    pub asset_files: usize,
    pub has_database: bool,
    pub has_viewer: bool,
    pub has_pages: bool,
}

/// Security expectations for the deployment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityExpectations {
    /// Whether COOP/COEP headers are configured (required for SQLite OPFS).
    pub cross_origin_isolation: bool,
    /// Whether the bundle contains a database (privacy consideration).
    pub contains_database: bool,
    /// Whether static pages are pre-rendered (no runtime data leakage).
    pub static_only: bool,
    /// Scrub preset used during export (if detectable from manifest).
    pub scrub_preset: Option<String>,
    /// Security notes and recommendations.
    pub notes: Vec<String>,
}

/// Rollback guidance for the deployment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackGuidance {
    /// Content hash of the current bundle.
    pub current_hash: Option<String>,
    /// Content hash of the previous deployment (if history available).
    pub previous_hash: Option<String>,
    /// Platform-specific rollback steps.
    pub steps: Vec<RollbackStep>,
}

/// A single rollback step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackStep {
    pub platform: String,
    pub instruction: String,
    pub command: Option<String>,
}

/// Post-deploy verification result for a live URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResult {
    pub url: String,
    pub checked_at: String,
    pub checks: Vec<DeployCheck>,
    pub all_passed: bool,
}

// ── Verify-live report (SPEC-verify-live-contract.md) ────────────────────

/// Overall verification verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyVerdict {
    /// All checks passed (or only info/skipped failures).
    Pass,
    /// No error-severity failures, but warning-severity failures exist.
    Warn,
    /// At least one error-severity check failed.
    Fail,
}

/// A single verify-live check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyLiveCheck {
    /// Dotted check identifier (e.g., `remote.root`).
    pub id: String,
    /// Human-readable check description.
    pub description: String,
    /// Severity of this check.
    pub severity: CheckSeverity,
    /// Whether the check passed.
    pub passed: bool,
    /// Result detail (success or failure reason).
    pub message: String,
    /// Time taken for this check in milliseconds.
    pub elapsed_ms: u64,
    /// HTTP response status code (remote checks only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// Relevant response headers (remote checks only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers_captured: Option<BTreeMap<String, String>>,
}

/// A verification stage (local, remote, or security).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyStage {
    /// Whether this stage was executed.
    pub ran: bool,
    /// Check results for this stage.
    pub checks: Vec<VerifyLiveCheck>,
}

/// Summary counts for a verify-live report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub warnings: usize,
    pub skipped: usize,
    pub elapsed_ms: u64,
}

/// Configuration used for a verify-live run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyConfig {
    pub strict: bool,
    pub fail_fast: bool,
    pub timeout_ms: u64,
    pub retries: u32,
    pub security_audit: bool,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            strict: false,
            fail_fast: false,
            timeout_ms: 10_000,
            retries: 2,
            security_audit: false,
        }
    }
}

/// Full verify-live report (SPEC-verify-live-contract.md schema_version 1.0.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyLiveReport {
    pub schema_version: String,
    pub generated_at: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<String>,
    pub verdict: VerifyVerdict,
    pub stages: VerifyStages,
    pub summary: VerifySummary,
    pub config: VerifyConfig,
}

/// Container for all verification stages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyStages {
    pub local: VerifyStage,
    pub remote: VerifyStage,
    pub security: VerifyStage,
}

impl VerifyLiveReport {
    /// Compute verdict from check results.
    #[must_use]
    pub fn compute_verdict(stages: &VerifyStages) -> VerifyVerdict {
        let all_checks = stages
            .local
            .checks
            .iter()
            .chain(stages.remote.checks.iter())
            .chain(stages.security.checks.iter());

        let mut has_error = false;
        let mut has_warning = false;

        for check in all_checks {
            if !check.passed {
                match check.severity {
                    CheckSeverity::Error => has_error = true,
                    CheckSeverity::Warning => has_warning = true,
                    CheckSeverity::Info | CheckSeverity::Skipped => {}
                }
            }
        }

        if has_error {
            VerifyVerdict::Fail
        } else if has_warning {
            VerifyVerdict::Warn
        } else {
            VerifyVerdict::Pass
        }
    }

    /// Compute summary counts from stages.
    #[must_use]
    pub fn compute_summary(stages: &VerifyStages, total_elapsed_ms: u64) -> VerifySummary {
        let all_checks: Vec<&VerifyLiveCheck> = stages
            .local
            .checks
            .iter()
            .chain(stages.remote.checks.iter())
            .chain(stages.security.checks.iter())
            .collect();

        let total = all_checks.len();
        let passed = all_checks.iter().filter(|c| c.passed).count();
        let skipped = all_checks
            .iter()
            .filter(|c| c.severity == CheckSeverity::Skipped)
            .count();
        let warnings = all_checks
            .iter()
            .filter(|c| !c.passed && c.severity == CheckSeverity::Warning)
            .count();
        let failed = all_checks
            .iter()
            .filter(|c| !c.passed && c.severity == CheckSeverity::Error)
            .count();

        VerifySummary {
            total,
            passed,
            failed,
            warnings,
            skipped,
            elapsed_ms: total_elapsed_ms,
        }
    }

    /// Determine exit code per SPEC-verify-live-contract.md.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self.verdict {
            VerifyVerdict::Fail => 1,
            VerifyVerdict::Warn if self.config.strict => 1,
            _ => 0,
        }
    }
}

// ── Verify-live orchestration ────────────────────────────────────────────

/// Options for a verify-live run.
#[derive(Debug, Clone, Default)]
pub struct VerifyLiveOptions {
    /// URL to verify.
    pub url: String,
    /// Local bundle directory (Stage 1). If None, Stage 1 is skipped.
    pub bundle_path: Option<std::path::PathBuf>,
    /// Run security header audit (Stage 3).
    pub security_audit: bool,
    /// Promote warnings to errors for exit code.
    pub strict: bool,
    /// Stop after first error-severity failure.
    pub fail_fast: bool,
    /// Probe configuration (timeout, retries, etc.).
    pub probe_config: crate::probe::ProbeConfig,
}

/// Map a `DeployCheck` from `validate_bundle()` into a `VerifyLiveCheck`.
fn map_bundle_check(check: &DeployCheck, elapsed_ms: u64) -> VerifyLiveCheck {
    VerifyLiveCheck {
        id: format!("bundle.{}", check.name),
        description: check.message.clone(),
        severity: check.severity,
        passed: check.passed,
        message: if check.passed {
            check.message.clone()
        } else {
            format!("FAIL: {}", check.message)
        },
        elapsed_ms,
        http_status: None,
        headers_captured: None,
    }
}

/// Map a `ProbeCheckResult` into a `VerifyLiveCheck`.
fn map_probe_result(result: &crate::probe::ProbeCheckResult) -> VerifyLiveCheck {
    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = result.elapsed.as_millis() as u64;
    VerifyLiveCheck {
        id: result.id.clone(),
        description: result.description.clone(),
        severity: result.severity,
        passed: result.passed,
        message: result.message.clone(),
        elapsed_ms,
        http_status: result.http_status,
        headers_captured: if result.headers_captured.is_empty() {
            None
        } else {
            Some(result.headers_captured.clone())
        },
    }
}

/// Build the standard remote probe checks per `SPEC-verify-live-contract.md`.
fn build_remote_checks() -> Vec<crate::probe::ProbeCheck> {
    vec![
        crate::probe::ProbeCheck {
            id: "remote.root".to_string(),
            description: "Root page accessible".to_string(),
            path: "/".to_string(),
            expected_status: Some(200),
            required_headers: vec![],
            severity: CheckSeverity::Error,
        },
        crate::probe::ProbeCheck {
            id: "remote.viewer".to_string(),
            description: "Viewer page accessible".to_string(),
            path: "/viewer/".to_string(),
            expected_status: Some(200),
            required_headers: vec![],
            severity: CheckSeverity::Warning,
        },
        crate::probe::ProbeCheck {
            id: "remote.manifest".to_string(),
            description: "Manifest accessible".to_string(),
            path: "/manifest.json".to_string(),
            expected_status: Some(200),
            required_headers: vec![],
            severity: CheckSeverity::Error,
        },
        crate::probe::ProbeCheck {
            id: "remote.coop".to_string(),
            description: "Cross-Origin-Opener-Policy header present".to_string(),
            path: "/".to_string(),
            expected_status: None,
            required_headers: vec!["Cross-Origin-Opener-Policy".to_string()],
            severity: CheckSeverity::Warning,
        },
        crate::probe::ProbeCheck {
            id: "remote.coep".to_string(),
            description: "Cross-Origin-Embedder-Policy header present".to_string(),
            path: "/".to_string(),
            expected_status: None,
            required_headers: vec!["Cross-Origin-Embedder-Policy".to_string()],
            severity: CheckSeverity::Warning,
        },
        crate::probe::ProbeCheck {
            id: "remote.database".to_string(),
            description: "Database accessible".to_string(),
            path: "/mailbox.sqlite3".to_string(),
            expected_status: Some(200),
            required_headers: vec![],
            severity: CheckSeverity::Info,
        },
    ]
}

/// Build the security audit checks per `SPEC-verify-live-contract.md`.
fn build_security_checks() -> Vec<crate::probe::ProbeCheck> {
    vec![
        crate::probe::ProbeCheck {
            id: "security.hsts".to_string(),
            description: "Strict-Transport-Security header".to_string(),
            path: "/".to_string(),
            expected_status: None,
            required_headers: vec!["Strict-Transport-Security".to_string()],
            severity: CheckSeverity::Info,
        },
        crate::probe::ProbeCheck {
            id: "security.x_content_type".to_string(),
            description: "X-Content-Type-Options header".to_string(),
            path: "/".to_string(),
            expected_status: None,
            required_headers: vec!["X-Content-Type-Options".to_string()],
            severity: CheckSeverity::Info,
        },
        crate::probe::ProbeCheck {
            id: "security.x_frame".to_string(),
            description: "X-Frame-Options header".to_string(),
            path: "/".to_string(),
            expected_status: None,
            required_headers: vec!["X-Frame-Options".to_string()],
            severity: CheckSeverity::Info,
        },
        crate::probe::ProbeCheck {
            id: "security.corp".to_string(),
            description: "Cross-Origin-Resource-Policy header".to_string(),
            path: "/".to_string(),
            expected_status: None,
            required_headers: vec!["Cross-Origin-Resource-Policy".to_string()],
            severity: CheckSeverity::Info,
        },
    ]
}

/// Check a header's exact value and produce a `VerifyLiveCheck`.
fn check_header_value(
    headers: &std::collections::BTreeMap<String, String>,
    id: &str,
    description: &str,
    header_key: &str,
    expected_value: &str,
    severity: CheckSeverity,
) -> VerifyLiveCheck {
    match headers.get(header_key) {
        Some(val) if val == expected_value => VerifyLiveCheck {
            id: id.to_string(),
            description: description.to_string(),
            severity,
            passed: true,
            message: format!("{header_key}: {val}"),
            elapsed_ms: 0,
            http_status: None,
            headers_captured: None,
        },
        Some(val) => VerifyLiveCheck {
            id: id.to_string(),
            description: description.to_string(),
            severity,
            passed: false,
            message: format!("{header_key} is \"{val}\", expected \"{expected_value}\""),
            elapsed_ms: 0,
            http_status: None,
            headers_captured: None,
        },
        None => VerifyLiveCheck {
            id: id.to_string(),
            description: description.to_string(),
            severity,
            passed: false,
            message: format!("{header_key} header missing"),
            elapsed_ms: 0,
            http_status: None,
            headers_captured: None,
        },
    }
}

/// Run the full verify-live pipeline (Stage 1 + Stage 2 + optional Stage 3).
///
/// Returns a complete `VerifyLiveReport` conforming to the JSON schema
/// defined in `SPEC-verify-live-contract.md`.
pub fn run_verify_live(opts: &VerifyLiveOptions) -> VerifyLiveReport {
    let start = std::time::Instant::now();

    // ── Stage 1: Local bundle validation ────────────────────────────
    let local_stage = if let Some(ref bundle_dir) = opts.bundle_path {
        let bundle_start = std::time::Instant::now();
        match validate_bundle(bundle_dir) {
            Ok(report) => {
                #[allow(clippy::cast_possible_truncation)]
                let elapsed = bundle_start.elapsed().as_millis() as u64;
                let checks: Vec<VerifyLiveCheck> = report
                    .checks
                    .iter()
                    .map(|c| map_bundle_check(c, elapsed))
                    .collect();

                // Check for fail-fast short-circuit
                let has_error = opts.fail_fast
                    && checks
                        .iter()
                        .any(|c| !c.passed && c.severity == CheckSeverity::Error);

                if has_error {
                    // Return early with only local stage
                    let stages = VerifyStages {
                        local: VerifyStage { ran: true, checks },
                        remote: VerifyStage {
                            ran: false,
                            checks: vec![],
                        },
                        security: VerifyStage {
                            ran: false,
                            checks: vec![],
                        },
                    };
                    let verdict = VerifyLiveReport::compute_verdict(&stages);
                    #[allow(clippy::cast_possible_truncation)]
                    let total_elapsed = start.elapsed().as_millis() as u64;
                    let summary = VerifyLiveReport::compute_summary(&stages, total_elapsed);
                    return VerifyLiveReport {
                        schema_version: "1.0.0".to_string(),
                        generated_at: Utc::now().to_rfc3339(),
                        url: opts.url.clone(),
                        bundle_path: Some(bundle_dir.display().to_string()),
                        verdict,
                        stages,
                        summary,
                        config: VerifyConfig {
                            strict: opts.strict,
                            fail_fast: opts.fail_fast,
                            timeout_ms: opts.probe_config.timeout.as_millis() as u64,
                            retries: opts.probe_config.retries,
                            security_audit: opts.security_audit,
                        },
                    };
                }

                VerifyStage { ran: true, checks }
            }
            Err(e) => VerifyStage {
                ran: true,
                checks: vec![VerifyLiveCheck {
                    id: "bundle.error".to_string(),
                    description: "Bundle validation failed".to_string(),
                    severity: CheckSeverity::Error,
                    passed: false,
                    message: e.to_string(),
                    elapsed_ms: 0,
                    http_status: None,
                    headers_captured: None,
                }],
            },
        }
    } else {
        VerifyStage {
            ran: false,
            checks: vec![],
        }
    };

    // ── Stage 2: Remote endpoint probes ─────────────────────────────
    let remote_checks = build_remote_checks();
    let remote_results =
        crate::probe::run_probe_checks(&opts.url, &remote_checks, &opts.probe_config);
    let mut remote_live_checks: Vec<VerifyLiveCheck> =
        remote_results.iter().map(map_probe_result).collect();

    // remote.content_match: SHA256 comparison (only when bundle provided and root passed)
    let root_result = remote_results.iter().find(|r| r.id == "remote.root");
    let root_passed = root_result.is_some_and(|r| r.passed);
    let content_match_check = if opts.bundle_path.is_some() && root_passed {
        let match_start = std::time::Instant::now();
        // Get remote body from root probe
        let remote_body_hash: Option<String> = {
            let root_url = format!("{}/", opts.url.trim_end_matches('/'));
            crate::probe::probe_get(&root_url, &opts.probe_config)
                .ok()
                .map(|resp| format!("{:x}", Sha256::digest(&resp.body)))
        };
        // Get local index.html hash
        let local_hash: Option<String> = opts.bundle_path.as_ref().and_then(|bp| {
            let index_path = bp.join("index.html");
            std::fs::read(&index_path)
                .ok()
                .map(|data| format!("{:x}", Sha256::digest(&data)))
        });
        #[allow(clippy::cast_possible_truncation)]
        let elapsed_ms = match_start.elapsed().as_millis() as u64;
        match (remote_body_hash, local_hash) {
            (Some(remote), Some(local)) if remote == local => VerifyLiveCheck {
                id: "remote.content_match".to_string(),
                description: "Root page content matches bundle".to_string(),
                severity: CheckSeverity::Warning,
                passed: true,
                message: format!("SHA256 match ({})", &remote[..12]),
                elapsed_ms,
                http_status: None,
                headers_captured: None,
            },
            (Some(remote), Some(local)) => VerifyLiveCheck {
                id: "remote.content_match".to_string(),
                description: "Root page content matches bundle".to_string(),
                severity: CheckSeverity::Warning,
                passed: false,
                message: format!(
                    "SHA256 mismatch: remote={}... local={}...",
                    &remote[..12],
                    &local[..12]
                ),
                elapsed_ms,
                http_status: None,
                headers_captured: None,
            },
            _ => VerifyLiveCheck {
                id: "remote.content_match".to_string(),
                description: "Root page content matches bundle".to_string(),
                severity: CheckSeverity::Skipped,
                passed: false,
                message: "could not compute hash for comparison".to_string(),
                elapsed_ms,
                http_status: None,
                headers_captured: None,
            },
        }
    } else {
        VerifyLiveCheck {
            id: "remote.content_match".to_string(),
            description: "Root page content matches bundle".to_string(),
            severity: CheckSeverity::Skipped,
            passed: false,
            message: if opts.bundle_path.is_none() {
                "skipped (no --bundle provided)".to_string()
            } else {
                "skipped (remote.root failed)".to_string()
            },
            elapsed_ms: 0,
            http_status: None,
            headers_captured: None,
        }
    };
    remote_live_checks.push(content_match_check);

    // remote.tls: HTTPS connection check (synthesized from root probe)
    let is_https = opts.url.starts_with("https://") || opts.url.starts_with("HTTPS://");
    let tls_check = if is_https {
        match root_result {
            Some(r) if r.passed => VerifyLiveCheck {
                id: "remote.tls".to_string(),
                description: "HTTPS connection succeeded".to_string(),
                severity: CheckSeverity::Error,
                passed: true,
                message: "HTTPS connection succeeded".to_string(),
                elapsed_ms: 0,
                http_status: None,
                headers_captured: None,
            },
            Some(r) => VerifyLiveCheck {
                id: "remote.tls".to_string(),
                description: "HTTPS connection succeeded".to_string(),
                severity: CheckSeverity::Error,
                passed: false,
                message: format!("HTTPS connection failed: {}", r.message),
                elapsed_ms: 0,
                http_status: r.http_status,
                headers_captured: None,
            },
            None => VerifyLiveCheck {
                id: "remote.tls".to_string(),
                description: "HTTPS connection succeeded".to_string(),
                severity: CheckSeverity::Error,
                passed: false,
                message: "root probe did not run".to_string(),
                elapsed_ms: 0,
                http_status: None,
                headers_captured: None,
            },
        }
    } else {
        VerifyLiveCheck {
            id: "remote.tls".to_string(),
            description: "HTTPS connection succeeded".to_string(),
            severity: CheckSeverity::Skipped,
            passed: false,
            message: "skipped (URL is not HTTPS)".to_string(),
            elapsed_ms: 0,
            http_status: None,
            headers_captured: None,
        }
    };
    remote_live_checks.push(tls_check);

    let remote_stage = VerifyStage {
        ran: true,
        checks: remote_live_checks,
    };

    // ── Stage 3: Security header audit ──────────────────────────────
    let security_stage = if opts.security_audit {
        let security_checks = build_security_checks();
        let security_results =
            crate::probe::run_probe_checks(&opts.url, &security_checks, &opts.probe_config);
        let mut sec_checks: Vec<VerifyLiveCheck> =
            security_results.iter().map(map_probe_result).collect();

        // Exact-value checks: COOP and COEP per SPEC.
        // Use headers captured from remote.coop / remote.coep (Stage 2).
        let coop_headers = remote_results
            .iter()
            .find(|r| r.id == "remote.coop")
            .map(|r| &r.headers_captured)
            .cloned()
            .unwrap_or_default();
        let coep_headers = remote_results
            .iter()
            .find(|r| r.id == "remote.coep")
            .map(|r| &r.headers_captured)
            .cloned()
            .unwrap_or_default();

        // security.coop_value: COOP must be "same-origin"
        sec_checks.push(check_header_value(
            &coop_headers,
            "security.coop_value",
            "COOP is same-origin",
            "cross-origin-opener-policy",
            "same-origin",
            CheckSeverity::Warning,
        ));
        // security.coep_value: COEP must be "require-corp"
        sec_checks.push(check_header_value(
            &coep_headers,
            "security.coep_value",
            "COEP is require-corp",
            "cross-origin-embedder-policy",
            "require-corp",
            CheckSeverity::Warning,
        ));

        VerifyStage {
            ran: true,
            checks: sec_checks,
        }
    } else {
        VerifyStage {
            ran: false,
            checks: vec![],
        }
    };

    // ── Assemble report ─────────────────────────────────────────────
    let stages = VerifyStages {
        local: local_stage,
        remote: remote_stage,
        security: security_stage,
    };

    let verdict = VerifyLiveReport::compute_verdict(&stages);
    #[allow(clippy::cast_possible_truncation)]
    let total_elapsed = start.elapsed().as_millis() as u64;
    let summary = VerifyLiveReport::compute_summary(&stages, total_elapsed);

    VerifyLiveReport {
        schema_version: "1.0.0".to_string(),
        generated_at: Utc::now().to_rfc3339(),
        url: opts.url.clone(),
        bundle_path: opts.bundle_path.as_ref().map(|p| p.display().to_string()),
        verdict,
        stages,
        summary,
        config: VerifyConfig {
            strict: opts.strict,
            fail_fast: opts.fail_fast,
            #[allow(clippy::cast_possible_truncation)]
            timeout_ms: opts.probe_config.timeout.as_millis() as u64,
            retries: opts.probe_config.retries,
            security_audit: opts.security_audit,
        },
    }
}

/// Deployment history entry for tracking deployments over time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployHistoryEntry {
    pub deployed_at: String,
    pub content_hash: String,
    pub platform: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

/// Deployment history stored alongside the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployHistory {
    pub entries: Vec<DeployHistoryEntry>,
}

// ── Pre-flight validation ───────────────────────────────────────────────

/// Run pre-flight validation checks on a bundle directory.
///
/// Returns a deployment report with check results, platform detection,
/// bundle statistics, and file integrity checksums.
pub fn validate_bundle(bundle_dir: &Path) -> ShareResult<DeployReport> {
    if !bundle_dir.is_dir() {
        return Err(ShareError::BundleNotFound {
            path: bundle_dir.display().to_string(),
        });
    }

    let mut checks = Vec::new();

    // ── Required files ──────────────────────────────────────────────
    check_file_exists(
        bundle_dir,
        "manifest.json",
        CheckSeverity::Error,
        &mut checks,
    );
    check_file_exists(bundle_dir, ".nojekyll", CheckSeverity::Warning, &mut checks);
    check_file_exists(bundle_dir, "_headers", CheckSeverity::Warning, &mut checks);
    check_file_exists(bundle_dir, "index.html", CheckSeverity::Error, &mut checks);

    // ── Viewer assets ───────────────────────────────────────────────
    check_file_exists(
        bundle_dir,
        "viewer/index.html",
        CheckSeverity::Warning,
        &mut checks,
    );
    check_file_exists(
        bundle_dir,
        "viewer/styles.css",
        CheckSeverity::Warning,
        &mut checks,
    );
    check_dir_exists(
        bundle_dir,
        "viewer/vendor",
        CheckSeverity::Warning,
        &mut checks,
    );

    // ── Database ────────────────────────────────────────────────────
    let has_db = bundle_dir.join("mailbox.sqlite3").is_file();
    checks.push(DeployCheck {
        name: "database_present".to_string(),
        passed: has_db,
        message: if has_db {
            "mailbox.sqlite3 found".to_string()
        } else {
            "mailbox.sqlite3 not found — viewer will have limited functionality".to_string()
        },
        severity: CheckSeverity::Warning,
    });

    // ── Static pages ────────────────────────────────────────────────
    let has_pages = bundle_dir.join("viewer/pages").is_dir();
    checks.push(DeployCheck {
        name: "static_pages_present".to_string(),
        passed: has_pages,
        message: if has_pages {
            "Pre-rendered HTML pages found".to_string()
        } else {
            "No pre-rendered pages — search engines won't index content".to_string()
        },
        severity: CheckSeverity::Info,
    });

    // ── Data files ──────────────────────────────────────────────────
    let _has_data = bundle_dir.join("viewer/data").is_dir();
    check_file_exists(
        bundle_dir,
        "viewer/data/messages.json",
        CheckSeverity::Warning,
        &mut checks,
    );
    check_file_exists(
        bundle_dir,
        "viewer/data/meta.json",
        CheckSeverity::Warning,
        &mut checks,
    );

    // ── Manifest validation ─────────────────────────────────────────
    let manifest_path = bundle_dir.join("manifest.json");
    if manifest_path.is_file() {
        match std::fs::read_to_string(&manifest_path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(manifest) => {
                    checks.push(DeployCheck {
                        name: "manifest_valid_json".to_string(),
                        passed: true,
                        message: "manifest.json is valid JSON".to_string(),
                        severity: CheckSeverity::Info,
                    });

                    // Check schema version
                    if let Some(version) = manifest.get("schema_version").and_then(|v| v.as_str()) {
                        checks.push(DeployCheck {
                            name: "manifest_schema_version".to_string(),
                            passed: true,
                            message: format!("Schema version: {version}"),
                            severity: CheckSeverity::Info,
                        });
                    }
                }
                Err(e) => {
                    checks.push(DeployCheck {
                        name: "manifest_valid_json".to_string(),
                        passed: false,
                        message: format!("manifest.json parse error: {e}"),
                        severity: CheckSeverity::Error,
                    });
                }
            },
            Err(e) => {
                checks.push(DeployCheck {
                    name: "manifest_readable".to_string(),
                    passed: false,
                    message: format!("Cannot read manifest.json: {e}"),
                    severity: CheckSeverity::Error,
                });
            }
        }
    }

    // ── Cross-origin headers check ──────────────────────────────────
    let headers_path = bundle_dir.join("_headers");
    if headers_path.is_file() {
        if let Ok(content) = std::fs::read_to_string(&headers_path) {
            let has_coop = content.contains("Cross-Origin-Opener-Policy");
            let has_coep = content.contains("Cross-Origin-Embedder-Policy");
            checks.push(DeployCheck {
                name: "coop_coep_headers".to_string(),
                passed: has_coop && has_coep,
                message: if has_coop && has_coep {
                    "COOP/COEP headers configured for cross-origin isolation".to_string()
                } else {
                    "Missing COOP or COEP headers — SQLite OPFS may not work".to_string()
                },
                severity: CheckSeverity::Warning,
            });
        }
    }

    // ── Platform detection ──────────────────────────────────────────
    let hosting_hints = crate::hosting::detect_hosting_hints(bundle_dir);
    let platforms = build_platform_info(bundle_dir, &hosting_hints);

    // ── Bundle stats ────────────────────────────────────────────────
    let bundle_stats = compute_bundle_stats(bundle_dir);

    // ── Integrity checksums ─────────────────────────────────────────
    let integrity = compute_integrity(bundle_dir);

    // ── Security expectations ───────────────────────────────────────
    let security = build_security_expectations(bundle_dir, &bundle_stats);

    // ── Rollback guidance ────────────────────────────────────────────
    let rollback = build_rollback_guidance(bundle_dir, &integrity);

    // ── Overall readiness ───────────────────────────────────────────
    let ready = !checks
        .iter()
        .any(|c| !c.passed && c.severity == CheckSeverity::Error);

    Ok(DeployReport {
        generated_at: Utc::now().to_rfc3339(),
        ready,
        checks,
        platforms,
        bundle_stats,
        integrity,
        security,
        rollback,
    })
}

// ── Platform config generators ──────────────────────────────────────────

/// Generate a GitHub Actions workflow for deploying to GitHub Pages.
#[must_use]
pub fn generate_gh_pages_workflow() -> String {
    r###"# Deploy MCP Agent Mail static export to GitHub Pages
#
# Usage:
#   1. Place your bundle output in the `docs/` directory (or configure path below)
#   2. Enable GitHub Pages in repo Settings > Pages > Source: "GitHub Actions"
#   3. Push to main branch to trigger deployment
#
# Or manually: Actions > "Deploy to GitHub Pages" > "Run workflow"

name: Deploy to GitHub Pages

on:
  push:
    branches: [main]
    paths:
      - 'docs/**'
  workflow_dispatch:

permissions:
  contents: read
  pages: write
  id-token: write

concurrency:
  group: pages
  cancel-in-progress: false

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Validate bundle
        run: |
          BUNDLE_DIR="docs"
          echo "=== Pre-flight checks ==="
          test -f "$BUNDLE_DIR/manifest.json" || { echo "FAIL: manifest.json missing"; exit 1; }
          test -f "$BUNDLE_DIR/index.html" || { echo "FAIL: index.html missing"; exit 1; }
          test -f "$BUNDLE_DIR/.nojekyll" || { echo "WARN: .nojekyll missing"; }
          test -f "$BUNDLE_DIR/_headers" || { echo "WARN: _headers missing"; }
          test -d "$BUNDLE_DIR/viewer" || { echo "WARN: viewer/ directory missing"; }
          echo "=== Manifest ==="
          python3 -c "import json; m=json.load(open('$BUNDLE_DIR/manifest.json')); print(json.dumps({k: m[k] for k in ['schema_version','generated_at','database'] if k in m}, indent=2))" 2>/dev/null || echo "(manifest parse skipped)"
          echo "=== Bundle size ==="
          du -sh "$BUNDLE_DIR"
          echo "=== All checks passed ==="

  deploy:
    needs: validate
    runs-on: ubuntu-latest
    environment:
      name: github-pages
      url: ${{ steps.deployment.outputs.page_url }}
    steps:
      - uses: actions/checkout@v4

      - name: Setup Pages
        uses: actions/configure-pages@v5

      - name: Upload artifact
        uses: actions/upload-pages-artifact@v3
        with:
          path: docs

      - name: Deploy to GitHub Pages
        id: deployment
        uses: actions/deploy-pages@v4

      - name: Generate deployment report
        if: always()
        run: |
          echo "## Deployment Report" >> $GITHUB_STEP_SUMMARY
          echo "- **Status**: ${{ steps.deployment.outcome }}" >> $GITHUB_STEP_SUMMARY
          echo "- **URL**: ${{ steps.deployment.outputs.page_url }}" >> $GITHUB_STEP_SUMMARY
          echo "- **Commit**: ${{ github.sha }}" >> $GITHUB_STEP_SUMMARY
          echo "- **Triggered by**: ${{ github.event_name }}" >> $GITHUB_STEP_SUMMARY
"###
    .to_string()
}

/// Generate a GitHub Actions workflow for deploying to Cloudflare Pages.
#[must_use]
pub fn generate_cf_pages_workflow() -> String {
    r###"# Deploy MCP Agent Mail static export to Cloudflare Pages
#
# Usage:
#   1. Set CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID secrets in repo settings
#   2. Create a Cloudflare Pages project: wrangler pages project create agent-mail
#   3. Push to main branch to trigger deployment
#
# Or manually: Actions > "Deploy to Cloudflare Pages" > "Run workflow"

name: Deploy to Cloudflare Pages

on:
  push:
    branches: [main]
    paths:
      - 'docs/**'
  workflow_dispatch:

concurrency:
  group: cf-pages
  cancel-in-progress: false

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Validate bundle
        run: |
          BUNDLE_DIR="docs"
          echo "=== Pre-flight checks ==="
          test -f "$BUNDLE_DIR/manifest.json" || { echo "FAIL: manifest.json missing"; exit 1; }
          test -f "$BUNDLE_DIR/index.html" || { echo "FAIL: index.html missing"; exit 1; }
          test -f "$BUNDLE_DIR/_headers" || { echo "WARN: _headers missing (COOP/COEP may be broken)"; }
          echo "=== Manifest ==="
          python3 -c "import json; m=json.load(open('$BUNDLE_DIR/manifest.json')); print(json.dumps({k: m[k] for k in ['schema_version','generated_at','database'] if k in m}, indent=2))" 2>/dev/null || echo "(manifest parse skipped)"
          echo "=== Bundle size ==="
          du -sh "$BUNDLE_DIR"
          echo "=== All checks passed ==="

  deploy:
    needs: validate
    runs-on: ubuntu-latest
    permissions:
      contents: read
      deployments: write
    steps:
      - uses: actions/checkout@v4

      - name: Deploy to Cloudflare Pages
        id: deploy
        uses: cloudflare/wrangler-action@v3
        with:
          apiToken: ${{ secrets.CLOUDFLARE_API_TOKEN }}
          accountId: ${{ secrets.CLOUDFLARE_ACCOUNT_ID }}
          command: pages deploy docs/ --project-name=agent-mail

      - name: Generate deployment report
        if: always()
        run: |
          echo "## Cloudflare Pages Deployment Report" >> $GITHUB_STEP_SUMMARY
          echo "- **Status**: ${{ steps.deploy.outcome }}" >> $GITHUB_STEP_SUMMARY
          echo "- **Commit**: ${{ github.sha }}" >> $GITHUB_STEP_SUMMARY
          echo "- **Triggered by**: ${{ github.event_name }}" >> $GITHUB_STEP_SUMMARY
"###
    .to_string()
}

/// Generate a Cloudflare Pages deployment configuration.
#[must_use]
pub fn generate_cf_pages_config() -> String {
    r#"# Cloudflare Pages Configuration
#
# This file is a template for wrangler.toml when deploying
# MCP Agent Mail static exports to Cloudflare Pages.
#
# Usage:
#   1. Install wrangler: npm install -g wrangler
#   2. Login: wrangler login
#   3. Deploy: wrangler pages deploy docs/ --project-name=agent-mail

name = "agent-mail-export"
compatibility_date = "2024-01-01"

[site]
bucket = "./docs"

# Cloudflare Pages automatically picks up _headers file
# for custom response headers (COOP/COEP configured there).
"#
    .to_string()
}

/// Generate a Netlify deployment configuration.
#[must_use]
pub fn generate_netlify_config() -> String {
    r#"# Netlify Configuration
#
# Place this file at the repo root when deploying to Netlify.
#
# Usage:
#   1. Connect your repo to Netlify
#   2. Set publish directory to "docs"
#   3. No build command needed (static files only)

[build]
  publish = "docs"

# Netlify automatically picks up _headers file
# for custom response headers (COOP/COEP configured there).

# Additional headers can be set here:
[[headers]]
  for = "/*.sqlite3"
  [headers.values]
    Content-Type = "application/x-sqlite3"
    Cross-Origin-Resource-Policy = "same-origin"

[[headers]]
  for = "/chunks/*"
  [headers.values]
    Content-Type = "application/octet-stream"
    Cross-Origin-Resource-Policy = "same-origin"
"#
    .to_string()
}

/// Generate a deployment validation script (shell).
#[must_use]
pub fn generate_validation_script() -> String {
    r#"#!/usr/bin/env bash
# MCP Agent Mail Static Export — Compatibility Validation Wrapper
#
# Usage: ./validate_deploy.sh <bundle_dir> [deployed_url]
#
# IMPORTANT:
#   Native command path is authoritative:
#     am share deploy verify-live <deployed_url> --bundle <bundle_dir>
#   This script is compatibility-only and may be removed in a future release.

set -euo pipefail

BUNDLE_DIR="${1:?Usage: $0 <bundle_dir> [deployed_url]}"
DEPLOYED_URL="${2:-}"

echo "=== MCP Agent Mail Deploy Validator (Compatibility Wrapper) ==="
echo "Bundle: $BUNDLE_DIR"
echo "Native path: am share deploy verify-live"
echo ""

if command -v am >/dev/null 2>&1; then
    if [ -n "$DEPLOYED_URL" ]; then
        CMD=(am share deploy verify-live "$DEPLOYED_URL" --bundle "$BUNDLE_DIR")
        if [ "${AM_VERIFY_LIVE_STRICT:-0}" = "1" ]; then
            CMD+=(--strict)
        fi
        echo "Delegating to native command:"
        printf '  %q ' "${CMD[@]}"
        echo ""
        exec "${CMD[@]}"
    fi

    echo "No deployed URL provided; running native bundle validation:"
    echo "  am share deploy validate \"$BUNDLE_DIR\""
    exec am share deploy validate "$BUNDLE_DIR"
fi

echo "WARNING: 'am' command not found; running compatibility fallback checks."
echo "Install/build the 'am' CLI for full verify-live behavior."
echo ""

ERRORS=0
WARNINGS=0

check() {
    local severity="$1" name="$2" condition="$3" msg_pass="$4" msg_fail="$5"
    if eval "$condition"; then
        echo "  ✅ $name: $msg_pass"
    else
        if [ "$severity" = "error" ]; then
            echo "  ❌ $name: $msg_fail"
            ERRORS=$((ERRORS + 1))
        else
            echo "  ⚠️  $name: $msg_fail"
            WARNINGS=$((WARNINGS + 1))
        fi
    fi
}

echo "--- Compatibility Structure Checks ---"
check error "manifest" "test -f \"$BUNDLE_DIR/manifest.json\"" "Present" "Missing"
check error "index.html" "test -f \"$BUNDLE_DIR/index.html\"" "Present" "Missing"
check warning ".nojekyll" "test -f \"$BUNDLE_DIR/.nojekyll\"" "Present" "Missing (needed for GH Pages)"
check warning "_headers" "test -f \"$BUNDLE_DIR/_headers\"" "Present" "Missing (needed for COOP/COEP)"
check warning "viewer" "test -d \"$BUNDLE_DIR/viewer\"" "Present" "Missing"
echo ""

if [ -n "$DEPLOYED_URL" ]; then
    echo "--- Compatibility HTTP Checks ($DEPLOYED_URL) ---"
    check_url() {
        local path="$1" expected="$2"
        local status
        status=$(curl -s -o /dev/null -w "%{http_code}" "$DEPLOYED_URL/$path" 2>/dev/null || echo "000")
        if [ "$status" = "$expected" ]; then
            echo "  ✅ GET /$path → $status"
        else
            echo "  ❌ GET /$path → $status (expected $expected)"
            ERRORS=$((ERRORS + 1))
        fi
    }
    check_url "" "200"
    check_url "viewer/" "200"
    check_url "manifest.json" "200"
fi

echo ""
echo "--- Migration Mapping ---"
echo "  Preferred: am share deploy verify-live <deployed_url> --bundle <bundle_dir>"
echo "  Fallback : ./validate_deploy.sh <bundle_dir> [deployed_url]"
echo "  Wrapper  : set AM_VERIFY_LIVE_STRICT=1 to add --strict while delegating"
echo ""

echo "=== Summary ==="
echo "  Errors:   $ERRORS"
echo "  Warnings: $WARNINGS"
if [ "$ERRORS" -gt 0 ]; then
    echo "  Result:   FAIL"
    exit 1
else
    echo "  Result:   PASS"
    exit 0
fi
"#
    .to_string()
}

// ── Internal helpers ────────────────────────────────────────────────────

fn check_file_exists(
    bundle_dir: &Path,
    relative_path: &str,
    severity: CheckSeverity,
    checks: &mut Vec<DeployCheck>,
) {
    let exists = bundle_dir.join(relative_path).is_file();
    checks.push(DeployCheck {
        name: format!("file_{}", relative_path.replace('/', "_")),
        passed: exists,
        message: if exists {
            format!("{relative_path} present")
        } else {
            format!("{relative_path} missing")
        },
        severity,
    });
}

fn check_dir_exists(
    bundle_dir: &Path,
    relative_path: &str,
    severity: CheckSeverity,
    checks: &mut Vec<DeployCheck>,
) {
    let exists = bundle_dir.join(relative_path).is_dir();
    checks.push(DeployCheck {
        name: format!("dir_{}", relative_path.replace('/', "_")),
        passed: exists,
        message: if exists {
            format!("{relative_path}/ present")
        } else {
            format!("{relative_path}/ missing")
        },
        severity,
    });
}

fn build_platform_info(
    bundle_dir: &Path,
    hosting_hints: &[crate::hosting::HostingHint],
) -> Vec<PlatformInfo> {
    let mut platforms = Vec::new();

    let detected_ids: Vec<&str> = hosting_hints.iter().map(|h| h.id.as_str()).collect();

    platforms.push(PlatformInfo {
        id: "github_pages".to_string(),
        name: "GitHub Pages".to_string(),
        detected: detected_ids.contains(&"github_pages"),
        config_present: bundle_dir.join(".nojekyll").is_file(),
        deploy_command: Some("gh-pages -d docs/".to_string()),
    });

    platforms.push(PlatformInfo {
        id: "cloudflare_pages".to_string(),
        name: "Cloudflare Pages".to_string(),
        detected: detected_ids.contains(&"cloudflare_pages"),
        config_present: bundle_dir.join("_headers").is_file(),
        deploy_command: Some("wrangler pages deploy docs/ --project-name=agent-mail".to_string()),
    });

    platforms.push(PlatformInfo {
        id: "netlify".to_string(),
        name: "Netlify".to_string(),
        detected: detected_ids.contains(&"netlify"),
        config_present: bundle_dir.join("_headers").is_file(),
        deploy_command: Some("netlify deploy --prod --dir=docs/".to_string()),
    });

    platforms.push(PlatformInfo {
        id: "s3".to_string(),
        name: "Amazon S3".to_string(),
        detected: detected_ids.contains(&"s3"),
        config_present: false,
        deploy_command: Some("aws s3 sync docs/ s3://your-bucket/ --delete".to_string()),
    });

    platforms
}

fn compute_bundle_stats(bundle_dir: &Path) -> BundleStats {
    let mut total_files = 0usize;
    let mut total_bytes = 0u64;
    let mut html_pages = 0usize;
    let mut data_files = 0usize;
    let mut asset_files = 0usize;

    if let Ok(entries) = walk_dir_recursive(bundle_dir) {
        for entry in entries {
            total_files += 1;
            total_bytes += entry.size;

            if entry.path.ends_with(".html") {
                html_pages += 1;
            } else if entry.path.ends_with(".json") {
                data_files += 1;
            } else {
                asset_files += 1;
            }
        }
    }

    BundleStats {
        total_files,
        total_bytes,
        html_pages,
        data_files,
        asset_files,
        has_database: bundle_dir.join("mailbox.sqlite3").is_file(),
        has_viewer: bundle_dir.join("viewer/index.html").is_file(),
        has_pages: bundle_dir.join("viewer/pages").is_dir(),
    }
}

fn compute_integrity(bundle_dir: &Path) -> BTreeMap<String, String> {
    let mut checksums = BTreeMap::new();

    // Checksum key files only (not all files, for performance)
    let key_files = [
        "manifest.json",
        "index.html",
        "mailbox.sqlite3",
        "viewer/index.html",
        "viewer/data/messages.json",
        "viewer/data/meta.json",
        "viewer/data/sitemap.json",
        "viewer/data/search_index.json",
    ];

    for rel in &key_files {
        let path = bundle_dir.join(rel);
        if path.is_file() {
            if let Ok(data) = std::fs::read(&path) {
                let hash = hex::encode(Sha256::digest(&data));
                checksums.insert((*rel).to_string(), hash);
            }
        }
    }

    checksums
}

struct FileEntry {
    path: String,
    size: u64,
}

fn walk_dir_recursive(dir: &Path) -> Result<Vec<FileEntry>, std::io::Error> {
    let mut entries = Vec::new();
    walk_dir_inner(dir, dir, &mut entries)?;
    Ok(entries)
}

fn walk_dir_inner(
    root: &Path,
    current: &Path,
    entries: &mut Vec<FileEntry>,
) -> Result<(), std::io::Error> {
    if !current.is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_dir_inner(root, &path, entries)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            entries.push(FileEntry { path: rel, size });
        }
    }
    Ok(())
}

// ── Security expectations ───────────────────────────────────────────────

fn build_security_expectations(bundle_dir: &Path, stats: &BundleStats) -> SecurityExpectations {
    let headers_path = bundle_dir.join("_headers");
    let cross_origin = if headers_path.is_file() {
        std::fs::read_to_string(&headers_path)
            .map(|c| {
                c.contains("Cross-Origin-Opener-Policy")
                    && c.contains("Cross-Origin-Embedder-Policy")
            })
            .unwrap_or(false)
    } else {
        false
    };

    let scrub_preset = bundle_dir
        .join("manifest.json")
        .is_file()
        .then(|| {
            std::fs::read_to_string(bundle_dir.join("manifest.json"))
                .ok()
                .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                .and_then(|m| {
                    m.get("scrub")
                        .and_then(|s| s.get("preset"))
                        .and_then(|p| p.as_str().map(|s| s.to_string()))
                        .or_else(|| {
                            m.get("export_config")
                                .and_then(|e| e.get("scrub_preset"))
                                .and_then(|p| p.as_str().map(|s| s.to_string()))
                        })
                })
        })
        .flatten();

    let mut notes = Vec::new();
    if stats.has_database {
        notes.push(
            "Bundle contains SQLite database — ensure scrub preset meets privacy requirements"
                .to_string(),
        );
    }
    if !cross_origin {
        notes.push(
            "COOP/COEP headers missing — SQLite OPFS will not work in the browser viewer"
                .to_string(),
        );
    }
    if !stats.has_pages {
        notes.push("No pre-rendered pages — content requires JavaScript for rendering".to_string());
    }
    if scrub_preset.as_deref() == Some("archive") {
        notes.push(
            "Archive scrub preset retains all data — suitable for private deployments only"
                .to_string(),
        );
    }

    SecurityExpectations {
        cross_origin_isolation: cross_origin,
        contains_database: stats.has_database,
        static_only: !stats.has_database && stats.has_pages,
        scrub_preset,
        notes,
    }
}

// ── Rollback guidance ──────────────────────────────────────────────────

fn build_rollback_guidance(
    bundle_dir: &Path,
    integrity: &BTreeMap<String, String>,
) -> RollbackGuidance {
    // Compute current content hash from integrity checksums.
    let current_hash = if integrity.is_empty() {
        None
    } else {
        let mut hasher = Sha256::new();
        for (k, v) in integrity {
            hasher.update(k.as_bytes());
            hasher.update(v.as_bytes());
        }
        Some(hex::encode(hasher.finalize()))
    };

    // Load previous hash from deploy history if available.
    let previous_hash = load_deploy_history(bundle_dir)
        .ok()
        .and_then(|h| h.entries.last().map(|e| e.content_hash.clone()));

    let steps = vec![
        RollbackStep {
            platform: "github_pages".to_string(),
            instruction: "Revert the docs/ directory to the previous commit and push".to_string(),
            command: Some("git revert HEAD -- docs/ && git push".to_string()),
        },
        RollbackStep {
            platform: "cloudflare_pages".to_string(),
            instruction: "Roll back to the previous deployment in the Cloudflare dashboard, or re-deploy from a previous commit".to_string(),
            command: Some("wrangler pages deployment rollback --project-name=agent-mail".to_string()),
        },
        RollbackStep {
            platform: "netlify".to_string(),
            instruction: "Use the Netlify dashboard to restore a previous deploy, or re-deploy from a previous commit".to_string(),
            command: Some("netlify deploy --prod --dir=docs/ # from previous commit checkout".to_string()),
        },
        RollbackStep {
            platform: "s3".to_string(),
            instruction: "Re-sync from a previous bundle snapshot".to_string(),
            command: Some("aws s3 sync <previous-bundle>/ s3://your-bucket/ --delete".to_string()),
        },
    ];

    RollbackGuidance {
        current_hash,
        previous_hash,
        steps,
    }
}

// ── Deploy history ─────────────────────────────────────────────────────

const DEPLOY_HISTORY_FILE: &str = ".deploy_history.json";

/// Load deployment history from the bundle directory.
pub fn load_deploy_history(bundle_dir: &Path) -> ShareResult<DeployHistory> {
    let path = bundle_dir.join(DEPLOY_HISTORY_FILE);
    if !path.is_file() {
        return Ok(DeployHistory {
            entries: Vec::new(),
        });
    }
    let content = std::fs::read_to_string(&path)?;
    serde_json::from_str(&content).map_err(|e| ShareError::ManifestParse {
        message: format!("deploy history parse error: {e}"),
    })
}

/// Append an entry to the deployment history and write it to disk.
pub fn record_deploy(bundle_dir: &Path, entry: DeployHistoryEntry) -> ShareResult<()> {
    let mut history = load_deploy_history(bundle_dir)?;
    history.entries.push(entry);
    // Keep only the last 50 entries.
    if history.entries.len() > 50 {
        let drain_count = history.entries.len() - 50;
        history.entries.drain(..drain_count);
    }
    let json = serde_json::to_string_pretty(&history).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(bundle_dir.join(DEPLOY_HISTORY_FILE), json)?;
    Ok(())
}

// ── Post-deploy verification ───────────────────────────────────────────

/// Build a verification plan for a deployed URL (returns the list of checks
/// that *would* be performed). Actual HTTP checks require a runtime client,
/// so this produces the check descriptions and expected status codes.
pub fn build_verify_plan(deployed_url: &str) -> VerifyResult {
    let url = deployed_url.trim_end_matches('/');
    let mut checks = Vec::new();

    // Root page
    checks.push(DeployCheck {
        name: "root_page".to_string(),
        passed: false,
        message: format!("GET {url}/ should return 200"),
        severity: CheckSeverity::Error,
    });

    // Viewer
    checks.push(DeployCheck {
        name: "viewer_page".to_string(),
        passed: false,
        message: format!("GET {url}/viewer/ should return 200"),
        severity: CheckSeverity::Error,
    });

    // Manifest
    checks.push(DeployCheck {
        name: "manifest_accessible".to_string(),
        passed: false,
        message: format!("GET {url}/manifest.json should return 200"),
        severity: CheckSeverity::Error,
    });

    // COOP/COEP headers
    checks.push(DeployCheck {
        name: "coop_header".to_string(),
        passed: false,
        message: format!("GET {url}/viewer/ should include Cross-Origin-Opener-Policy header"),
        severity: CheckSeverity::Warning,
    });
    checks.push(DeployCheck {
        name: "coep_header".to_string(),
        passed: false,
        message: format!("GET {url}/viewer/ should include Cross-Origin-Embedder-Policy header"),
        severity: CheckSeverity::Warning,
    });

    // Database (if present)
    checks.push(DeployCheck {
        name: "database_accessible".to_string(),
        passed: false,
        message: format!("GET {url}/mailbox.sqlite3 should return 200 (if database included)"),
        severity: CheckSeverity::Info,
    });

    VerifyResult {
        url: url.to_string(),
        checked_at: Utc::now().to_rfc3339(),
        checks,
        all_passed: false,
    }
}

// ── Write workflow files to disk ────────────────────────────────────────

/// Write all deployment configuration files to a bundle directory.
///
/// Creates:
/// - `.github/workflows/deploy-pages.yml` (GitHub Actions workflow)
/// - `.github/workflows/deploy-cf-pages.yml` (Cloudflare Pages CI workflow)
/// - `wrangler.toml.template` (Cloudflare Pages config)
/// - `netlify.toml.template` (Netlify config)
/// - `scripts/validate_deploy.sh` (compatibility wrapper to native `am` validation commands)
/// - `deploy_report.json` (pre-flight validation report)
pub fn write_deploy_tooling(bundle_dir: &Path) -> ShareResult<Vec<String>> {
    let mut written = Vec::new();

    // GitHub Actions workflow (GH Pages)
    let workflow_dir = bundle_dir.join(".github").join("workflows");
    std::fs::create_dir_all(&workflow_dir)?;
    std::fs::write(
        workflow_dir.join("deploy-pages.yml"),
        generate_gh_pages_workflow(),
    )?;
    written.push(".github/workflows/deploy-pages.yml".to_string());

    // GitHub Actions workflow (Cloudflare Pages)
    std::fs::write(
        workflow_dir.join("deploy-cf-pages.yml"),
        generate_cf_pages_workflow(),
    )?;
    written.push(".github/workflows/deploy-cf-pages.yml".to_string());

    // Cloudflare Pages template
    std::fs::write(
        bundle_dir.join("wrangler.toml.template"),
        generate_cf_pages_config(),
    )?;
    written.push("wrangler.toml.template".to_string());

    // Netlify template
    std::fs::write(
        bundle_dir.join("netlify.toml.template"),
        generate_netlify_config(),
    )?;
    written.push("netlify.toml.template".to_string());

    // Validation script
    let scripts_dir = bundle_dir.join("scripts");
    std::fs::create_dir_all(&scripts_dir)?;
    let script_path = scripts_dir.join("validate_deploy.sh");
    std::fs::write(&script_path, generate_validation_script())?;
    // Make executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));
    }
    written.push("scripts/validate_deploy.sh".to_string());

    // Deploy report
    let report = validate_bundle(bundle_dir)?;
    let report_json = serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(bundle_dir.join("deploy_report.json"), &report_json)?;
    written.push("deploy_report.json".to_string());

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

    struct TestHttpServer {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl TestHttpServer {
        fn spawn(include_isolation_headers: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            listener
                .set_nonblocking(true)
                .expect("set_nonblocking true");
            let addr = listener.local_addr().expect("local_addr");
            let stop = Arc::new(AtomicBool::new(false));
            let stop_flag = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            serve_connection(stream, include_isolation_headers);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                addr,
                stop,
                handle: Some(handle),
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    impl Drop for TestHttpServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = TcpStream::connect(self.addr);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn serve_connection(mut stream: TcpStream, include_isolation_headers: bool) {
        let mut buf = [0_u8; 4096];
        let read = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(_) => return,
        };
        if read == 0 {
            return;
        }
        let req = String::from_utf8_lossy(&buf[..read]);
        let first_line = req.lines().next().unwrap_or_default();
        let path = first_line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string();

        let (status, body, mut headers) = match path.as_str() {
            "/" => {
                let mut h = BTreeMap::new();
                if include_isolation_headers {
                    h.insert(
                        "cross-origin-opener-policy".to_string(),
                        "same-origin".to_string(),
                    );
                    h.insert(
                        "cross-origin-embedder-policy".to_string(),
                        "require-corp".to_string(),
                    );
                    h.insert(
                        "strict-transport-security".to_string(),
                        "max-age=31536000".to_string(),
                    );
                    h.insert("x-content-type-options".to_string(), "nosniff".to_string());
                    h.insert("x-frame-options".to_string(), "DENY".to_string());
                    h.insert(
                        "cross-origin-resource-policy".to_string(),
                        "same-origin".to_string(),
                    );
                }
                (200_u16, "<html></html>".to_string(), h)
            }
            "/viewer/" => (200_u16, "<html>viewer</html>".to_string(), BTreeMap::new()),
            "/manifest.json" => (
                200_u16,
                "{\"schema_version\":\"0.1.0\"}".to_string(),
                BTreeMap::new(),
            ),
            "/mailbox.sqlite3" => (200_u16, "not-a-real-db".to_string(), BTreeMap::new()),
            _ => (404_u16, "not found".to_string(), BTreeMap::new()),
        };

        headers.insert(
            "content-type".to_string(),
            "text/html; charset=utf-8".to_string(),
        );
        headers.insert("connection".to_string(), "close".to_string());
        headers.insert("content-length".to_string(), body.len().to_string());

        let status_text = if status == 200 { "OK" } else { "Not Found" };
        let mut response = format!("HTTP/1.1 {status} {status_text}\r\n");
        for (k, v) in headers {
            response.push_str(&format!("{k}: {v}\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(&body);
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    fn create_minimal_bundle(dir: &Path) {
        std::fs::create_dir_all(dir.join("viewer/vendor")).unwrap();
        std::fs::create_dir_all(dir.join("viewer/data")).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"schema_version":"0.1.0","generated_at":"2024-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("index.html"), "<html></html>").unwrap();
        std::fs::write(dir.join(".nojekyll"), "").unwrap();
        std::fs::write(
            dir.join("_headers"),
            "Cross-Origin-Opener-Policy: same-origin\nCross-Origin-Embedder-Policy: require-corp",
        )
        .unwrap();
        std::fs::write(dir.join("viewer/index.html"), "<html>viewer</html>").unwrap();
        std::fs::write(dir.join("viewer/styles.css"), "body{}").unwrap();
        std::fs::write(dir.join("viewer/data/messages.json"), "[]").unwrap();
        std::fs::write(dir.join("viewer/data/meta.json"), "{}").unwrap();
    }

    // ── validate_bundle ─────────────────────────────────────────────

    #[test]
    fn validate_complete_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let report = validate_bundle(&bundle).unwrap();
        assert!(report.ready);
        assert!(
            report
                .checks
                .iter()
                .all(|c| c.passed || c.severity != CheckSeverity::Error)
        );
    }

    #[test]
    fn validate_missing_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("index.html"), "").unwrap();

        let report = validate_bundle(&bundle).unwrap();
        assert!(!report.ready);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "file_manifest.json" && !c.passed)
        );
    }

    #[test]
    fn validate_nonexistent_dir() {
        let result = validate_bundle(Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }

    // ── bundle stats ────────────────────────────────────────────────

    #[test]
    fn bundle_stats_counts() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let stats = compute_bundle_stats(&bundle);
        assert!(stats.total_files > 0);
        assert!(stats.total_bytes > 0);
        assert!(stats.has_viewer);
        assert!(!stats.has_database);
    }

    // ── integrity checksums ─────────────────────────────────────────

    #[test]
    fn integrity_checksums_computed() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let integrity = compute_integrity(&bundle);
        assert!(integrity.contains_key("manifest.json"));
        assert!(integrity.contains_key("index.html"));
        // SHA256 hex is 64 chars
        for hash in integrity.values() {
            assert_eq!(hash.len(), 64);
        }
    }

    // ── config generators ───────────────────────────────────────────

    #[test]
    fn gh_pages_workflow_is_valid_yaml() {
        let workflow = generate_gh_pages_workflow();
        assert!(workflow.contains("Deploy to GitHub Pages"));
        assert!(workflow.contains("actions/deploy-pages@v4"));
        assert!(workflow.contains("permissions:"));
    }

    #[test]
    fn cf_pages_config_valid() {
        let config = generate_cf_pages_config();
        assert!(config.contains("wrangler"));
        assert!(config.contains("compatibility_date"));
    }

    #[test]
    fn netlify_config_valid() {
        let config = generate_netlify_config();
        assert!(config.contains("[build]"));
        assert!(config.contains("publish"));
    }

    #[test]
    fn validation_script_is_shell() {
        let script = generate_validation_script();
        assert!(script.starts_with("#!/usr/bin/env bash"));
        assert!(script.contains("am share deploy verify-live"));
        assert!(script.contains("compatibility-only"));
        assert!(script.contains("check_url"));
    }

    // ── write_deploy_tooling ────────────────────────────────────────

    #[test]
    fn write_deploy_tooling_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let written = write_deploy_tooling(&bundle).unwrap();
        assert!(written.contains(&".github/workflows/deploy-pages.yml".to_string()));
        assert!(written.contains(&".github/workflows/deploy-cf-pages.yml".to_string()));
        assert!(written.contains(&"wrangler.toml.template".to_string()));
        assert!(written.contains(&"netlify.toml.template".to_string()));
        assert!(written.contains(&"scripts/validate_deploy.sh".to_string()));
        assert!(written.contains(&"deploy_report.json".to_string()));

        // Verify files exist
        assert!(bundle.join(".github/workflows/deploy-pages.yml").is_file());
        assert!(
            bundle
                .join(".github/workflows/deploy-cf-pages.yml")
                .is_file()
        );
        assert!(bundle.join("wrangler.toml.template").is_file());
        assert!(bundle.join("netlify.toml.template").is_file());
        assert!(bundle.join("scripts/validate_deploy.sh").is_file());
        assert!(bundle.join("deploy_report.json").is_file());

        // Verify deploy report is valid JSON with new fields
        let report_json = std::fs::read_to_string(bundle.join("deploy_report.json")).unwrap();
        let report: DeployReport = serde_json::from_str(&report_json).unwrap();
        assert!(report.ready);
        assert!(!report.generated_at.is_empty());
        assert!(!report.rollback.steps.is_empty());
    }

    // ── platform info ───────────────────────────────────────────────

    #[test]
    fn platform_info_includes_all_providers() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let platforms = build_platform_info(&bundle, &[]);
        assert_eq!(platforms.len(), 4);
        let ids: Vec<&str> = platforms.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"github_pages"));
        assert!(ids.contains(&"cloudflare_pages"));
        assert!(ids.contains(&"netlify"));
        assert!(ids.contains(&"s3"));
    }

    // ── deploy report serialization ─────────────────────────────────

    #[test]
    fn deploy_report_round_trips() {
        let report = DeployReport {
            generated_at: "2024-01-01T00:00:00Z".to_string(),
            ready: true,
            checks: vec![DeployCheck {
                name: "test".to_string(),
                passed: true,
                message: "ok".to_string(),
                severity: CheckSeverity::Info,
            }],
            platforms: vec![],
            bundle_stats: BundleStats {
                total_files: 10,
                total_bytes: 1000,
                html_pages: 5,
                data_files: 3,
                asset_files: 2,
                has_database: true,
                has_viewer: true,
                has_pages: true,
            },
            integrity: BTreeMap::new(),
            security: SecurityExpectations {
                cross_origin_isolation: true,
                contains_database: true,
                static_only: false,
                scrub_preset: Some("standard".to_string()),
                notes: vec!["test note".to_string()],
            },
            rollback: RollbackGuidance {
                current_hash: Some("abc123".to_string()),
                previous_hash: None,
                steps: vec![],
            },
        };

        let json = serde_json::to_string(&report).unwrap();
        let parsed: DeployReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.ready, report.ready);
        assert_eq!(parsed.bundle_stats.total_files, 10);
        assert!(parsed.security.cross_origin_isolation);
        assert_eq!(parsed.rollback.current_hash.as_deref(), Some("abc123"));
    }

    // ── CF Pages workflow ────────────────────────────────────────────

    #[test]
    fn cf_pages_workflow_is_valid_yaml() {
        let workflow = generate_cf_pages_workflow();
        assert!(workflow.contains("Deploy to Cloudflare Pages"));
        assert!(workflow.contains("cloudflare/wrangler-action@v3"));
        assert!(workflow.contains("CLOUDFLARE_API_TOKEN"));
    }

    // ── Security expectations ────────────────────────────────────────

    #[test]
    fn security_expectations_with_database() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);
        // Add a database
        std::fs::write(bundle.join("mailbox.sqlite3"), b"fake-db").unwrap();

        let stats = compute_bundle_stats(&bundle);
        let security = build_security_expectations(&bundle, &stats);
        assert!(security.cross_origin_isolation);
        assert!(security.contains_database);
        assert!(!security.static_only);
        assert!(security.notes.iter().any(|n| n.contains("SQLite database")));
    }

    #[test]
    fn security_expectations_static_only() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);
        std::fs::create_dir_all(bundle.join("viewer/pages")).unwrap();
        std::fs::write(bundle.join("viewer/pages/index.html"), "<html/>").unwrap();

        let stats = compute_bundle_stats(&bundle);
        let security = build_security_expectations(&bundle, &stats);
        assert!(!security.contains_database);
        assert!(security.static_only);
    }

    #[test]
    fn security_expectations_no_headers() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("index.html"), "").unwrap();

        let stats = compute_bundle_stats(&bundle);
        let security = build_security_expectations(&bundle, &stats);
        assert!(!security.cross_origin_isolation);
        assert!(security.notes.iter().any(|n| n.contains("COOP/COEP")));
    }

    // ── Rollback guidance ────────────────────────────────────────────

    #[test]
    fn rollback_has_all_platforms() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let integrity = compute_integrity(&bundle);
        let rollback = build_rollback_guidance(&bundle, &integrity);
        assert!(rollback.current_hash.is_some());
        assert!(rollback.previous_hash.is_none());
        assert_eq!(rollback.steps.len(), 4);
        let platforms: Vec<&str> = rollback.steps.iter().map(|s| s.platform.as_str()).collect();
        assert!(platforms.contains(&"github_pages"));
        assert!(platforms.contains(&"cloudflare_pages"));
        assert!(platforms.contains(&"netlify"));
        assert!(platforms.contains(&"s3"));
    }

    // ── Deploy history ───────────────────────────────────────────────

    #[test]
    fn deploy_history_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();

        // Initially empty.
        let history = load_deploy_history(&bundle).unwrap();
        assert!(history.entries.is_empty());

        // Record an entry.
        record_deploy(
            &bundle,
            DeployHistoryEntry {
                deployed_at: "2024-01-01T00:00:00Z".to_string(),
                content_hash: "abc123".to_string(),
                platform: "github_pages".to_string(),
                file_count: 10,
                total_bytes: 1000,
            },
        )
        .unwrap();

        let history = load_deploy_history(&bundle).unwrap();
        assert_eq!(history.entries.len(), 1);
        assert_eq!(history.entries[0].content_hash, "abc123");

        // Record a second entry.
        record_deploy(
            &bundle,
            DeployHistoryEntry {
                deployed_at: "2024-01-02T00:00:00Z".to_string(),
                content_hash: "def456".to_string(),
                platform: "cloudflare_pages".to_string(),
                file_count: 12,
                total_bytes: 1200,
            },
        )
        .unwrap();

        let history = load_deploy_history(&bundle).unwrap();
        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.entries[1].content_hash, "def456");
    }

    // ── Verify plan ──────────────────────────────────────────────────

    #[test]
    fn verify_plan_has_expected_checks() {
        let plan = build_verify_plan("https://example.com/site");
        assert_eq!(plan.url, "https://example.com/site");
        assert!(!plan.all_passed);
        assert!(plan.checks.len() >= 5);
        let names: Vec<&str> = plan.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"root_page"));
        assert!(names.contains(&"viewer_page"));
        assert!(names.contains(&"manifest_accessible"));
        assert!(names.contains(&"coop_header"));
        assert!(names.contains(&"coep_header"));
    }

    #[test]
    fn verify_plan_strips_trailing_slash() {
        let plan = build_verify_plan("https://example.com/site/");
        assert_eq!(plan.url, "https://example.com/site");
    }

    // ── write_deploy_tooling includes CF workflow ────────────────────

    #[test]
    fn write_deploy_tooling_includes_cf_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let written = write_deploy_tooling(&bundle).unwrap();
        assert!(written.contains(&".github/workflows/deploy-cf-pages.yml".to_string()));
        assert!(
            bundle
                .join(".github/workflows/deploy-cf-pages.yml")
                .is_file()
        );
    }

    // ── Full report includes new fields ──────────────────────────────

    #[test]
    fn full_report_has_security_and_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let report = validate_bundle(&bundle).unwrap();
        assert!(!report.generated_at.is_empty());
        assert!(report.security.cross_origin_isolation);
        assert!(!report.security.contains_database);
        assert!(report.rollback.steps.len() >= 3);
    }

    // ── verify-live contract types ──────────────────────────────────

    fn make_check(id: &str, passed: bool, severity: CheckSeverity) -> VerifyLiveCheck {
        VerifyLiveCheck {
            id: id.to_string(),
            description: format!("check {id}"),
            severity,
            passed,
            message: if passed {
                "ok".to_string()
            } else {
                "failed".to_string()
            },
            elapsed_ms: 10,
            http_status: None,
            headers_captured: None,
        }
    }

    fn make_stages(checks: Vec<VerifyLiveCheck>) -> VerifyStages {
        VerifyStages {
            local: VerifyStage {
                ran: false,
                checks: vec![],
            },
            remote: VerifyStage { ran: true, checks },
            security: VerifyStage {
                ran: false,
                checks: vec![],
            },
        }
    }

    #[test]
    fn verdict_all_pass() {
        let stages = make_stages(vec![
            make_check("remote.root", true, CheckSeverity::Error),
            make_check("remote.viewer", true, CheckSeverity::Warning),
        ]);
        assert_eq!(
            VerifyLiveReport::compute_verdict(&stages),
            VerifyVerdict::Pass
        );
    }

    #[test]
    fn verdict_warning_only() {
        let stages = make_stages(vec![
            make_check("remote.root", true, CheckSeverity::Error),
            make_check("remote.coop", false, CheckSeverity::Warning),
        ]);
        assert_eq!(
            VerifyLiveReport::compute_verdict(&stages),
            VerifyVerdict::Warn
        );
    }

    #[test]
    fn verdict_error_failure() {
        let stages = make_stages(vec![
            make_check("remote.root", false, CheckSeverity::Error),
            make_check("remote.coop", false, CheckSeverity::Warning),
        ]);
        assert_eq!(
            VerifyLiveReport::compute_verdict(&stages),
            VerifyVerdict::Fail
        );
    }

    #[test]
    fn verdict_skipped_does_not_affect() {
        let stages = make_stages(vec![
            make_check("remote.root", true, CheckSeverity::Error),
            make_check("remote.content_match", false, CheckSeverity::Skipped),
        ]);
        assert_eq!(
            VerifyLiveReport::compute_verdict(&stages),
            VerifyVerdict::Pass
        );
    }

    #[test]
    fn verdict_info_failure_does_not_affect() {
        let stages = make_stages(vec![
            make_check("remote.root", true, CheckSeverity::Error),
            make_check("remote.database", false, CheckSeverity::Info),
        ]);
        assert_eq!(
            VerifyLiveReport::compute_verdict(&stages),
            VerifyVerdict::Pass
        );
    }

    #[test]
    fn summary_counts() {
        let stages = make_stages(vec![
            make_check("remote.root", true, CheckSeverity::Error),
            make_check("remote.viewer", true, CheckSeverity::Warning),
            make_check("remote.coop", false, CheckSeverity::Warning),
            make_check("remote.tls", false, CheckSeverity::Error),
            make_check("remote.content_match", false, CheckSeverity::Skipped),
        ]);
        let summary = VerifyLiveReport::compute_summary(&stages, 500);
        assert_eq!(summary.total, 5);
        assert_eq!(summary.passed, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.warnings, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.elapsed_ms, 500);
    }

    #[test]
    fn exit_code_pass() {
        let report = VerifyLiveReport {
            schema_version: "1.0.0".to_string(),
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://example.com".to_string(),
            bundle_path: None,
            verdict: VerifyVerdict::Pass,
            stages: make_stages(vec![]),
            summary: VerifySummary {
                total: 0,
                passed: 0,
                failed: 0,
                warnings: 0,
                skipped: 0,
                elapsed_ms: 0,
            },
            config: VerifyConfig::default(),
        };
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn exit_code_fail() {
        let report = VerifyLiveReport {
            schema_version: "1.0.0".to_string(),
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://example.com".to_string(),
            bundle_path: None,
            verdict: VerifyVerdict::Fail,
            stages: make_stages(vec![]),
            summary: VerifySummary {
                total: 1,
                passed: 0,
                failed: 1,
                warnings: 0,
                skipped: 0,
                elapsed_ms: 100,
            },
            config: VerifyConfig::default(),
        };
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn exit_code_warn_not_strict() {
        let report = VerifyLiveReport {
            schema_version: "1.0.0".to_string(),
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://example.com".to_string(),
            bundle_path: None,
            verdict: VerifyVerdict::Warn,
            stages: make_stages(vec![]),
            summary: VerifySummary {
                total: 1,
                passed: 0,
                failed: 0,
                warnings: 1,
                skipped: 0,
                elapsed_ms: 100,
            },
            config: VerifyConfig::default(),
        };
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn exit_code_warn_strict() {
        let config = VerifyConfig {
            strict: true,
            ..VerifyConfig::default()
        };
        let report = VerifyLiveReport {
            schema_version: "1.0.0".to_string(),
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://example.com".to_string(),
            bundle_path: None,
            verdict: VerifyVerdict::Warn,
            stages: make_stages(vec![]),
            summary: VerifySummary {
                total: 1,
                passed: 0,
                failed: 0,
                warnings: 1,
                skipped: 0,
                elapsed_ms: 100,
            },
            config,
        };
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn verify_config_defaults() {
        let config = VerifyConfig::default();
        assert!(!config.strict);
        assert!(!config.fail_fast);
        assert_eq!(config.timeout_ms, 10_000);
        assert_eq!(config.retries, 2);
        assert!(!config.security_audit);
    }

    #[test]
    fn verify_live_report_json_roundtrip() {
        let stages = make_stages(vec![make_check("remote.root", true, CheckSeverity::Error)]);
        let report = VerifyLiveReport {
            schema_version: "1.0.0".to_string(),
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://example.com".to_string(),
            bundle_path: Some("/path/to/bundle".to_string()),
            verdict: VerifyVerdict::Pass,
            stages,
            summary: VerifySummary {
                total: 1,
                passed: 1,
                failed: 0,
                warnings: 0,
                skipped: 0,
                elapsed_ms: 142,
            },
            config: VerifyConfig::default(),
        };

        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: VerifyLiveReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.schema_version, "1.0.0");
        assert_eq!(parsed.url, "https://example.com");
        assert_eq!(parsed.verdict, VerifyVerdict::Pass);
        assert_eq!(parsed.summary.total, 1);
        assert_eq!(parsed.summary.passed, 1);
        assert!(parsed.bundle_path.is_some());
    }

    #[test]
    fn check_severity_skipped_serializes() {
        let check = make_check("remote.content_match", false, CheckSeverity::Skipped);
        let json = serde_json::to_string(&check).unwrap();
        assert!(json.contains("\"skipped\""));
    }

    #[test]
    fn verify_live_check_with_http_fields() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "content-type".to_string(),
            "text/html; charset=utf-8".to_string(),
        );
        let check = VerifyLiveCheck {
            id: "remote.root".to_string(),
            description: "Root page accessible".to_string(),
            severity: CheckSeverity::Error,
            passed: true,
            message: "GET / → 200 (142ms)".to_string(),
            elapsed_ms: 142,
            http_status: Some(200),
            headers_captured: Some(headers),
        };
        let json = serde_json::to_string_pretty(&check).unwrap();
        assert!(json.contains("\"http_status\": 200"));
        assert!(json.contains("\"headers_captured\""));
        assert!(json.contains("text/html"));
    }

    #[test]
    fn verify_live_check_omits_none_http_fields() {
        let check = make_check("bundle.manifest", true, CheckSeverity::Error);
        let json = serde_json::to_string(&check).unwrap();
        assert!(!json.contains("http_status"));
        assert!(!json.contains("headers_captured"));
    }

    // ── check_header_value tests ────────────────────────────────────

    #[test]
    fn check_header_value_exact_match() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "cross-origin-opener-policy".to_string(),
            "same-origin".to_string(),
        );
        let result = super::check_header_value(
            &headers,
            "security.coop_value",
            "COOP is same-origin",
            "cross-origin-opener-policy",
            "same-origin",
            CheckSeverity::Warning,
        );
        assert!(result.passed);
        assert!(result.message.contains("same-origin"));
    }

    #[test]
    fn check_header_value_wrong_value() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "cross-origin-opener-policy".to_string(),
            "unsafe-none".to_string(),
        );
        let result = super::check_header_value(
            &headers,
            "security.coop_value",
            "COOP is same-origin",
            "cross-origin-opener-policy",
            "same-origin",
            CheckSeverity::Warning,
        );
        assert!(!result.passed);
        assert!(result.message.contains("unsafe-none"));
        assert!(result.message.contains("same-origin"));
    }

    #[test]
    fn check_header_value_missing_header() {
        let headers = BTreeMap::new();
        let result = super::check_header_value(
            &headers,
            "security.coep_value",
            "COEP is require-corp",
            "cross-origin-embedder-policy",
            "require-corp",
            CheckSeverity::Warning,
        );
        assert!(!result.passed);
        assert!(result.message.contains("missing"));
    }

    #[test]
    fn content_match_skipped_without_bundle() {
        // When bundle_path is None, content_match should be Skipped
        let check = VerifyLiveCheck {
            id: "remote.content_match".to_string(),
            description: "Root page content matches bundle".to_string(),
            severity: CheckSeverity::Skipped,
            passed: false,
            message: "skipped (no --bundle provided)".to_string(),
            elapsed_ms: 0,
            http_status: None,
            headers_captured: None,
        };
        assert_eq!(check.severity, CheckSeverity::Skipped);
        assert!(!check.passed);
    }

    #[test]
    fn tls_check_skipped_for_http() {
        let check = VerifyLiveCheck {
            id: "remote.tls".to_string(),
            description: "HTTPS connection succeeded".to_string(),
            severity: CheckSeverity::Skipped,
            passed: false,
            message: "skipped (URL is not HTTPS)".to_string(),
            elapsed_ms: 0,
            http_status: None,
            headers_captured: None,
        };
        assert_eq!(check.severity, CheckSeverity::Skipped);
    }

    #[test]
    fn verify_live_options_default() {
        let opts = VerifyLiveOptions::default();
        assert!(opts.url.is_empty());
        assert!(opts.bundle_path.is_none());
        assert!(!opts.security_audit);
        assert!(!opts.strict);
        assert!(!opts.fail_fast);
    }

    #[test]
    fn run_verify_live_integration_pass_with_security_and_content_match() {
        let server = TestHttpServer::spawn(true);
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let opts = VerifyLiveOptions {
            url: server.base_url(),
            bundle_path: Some(bundle),
            security_audit: true,
            strict: false,
            fail_fast: false,
            probe_config: crate::probe::ProbeConfig {
                timeout: Duration::from_secs(2),
                retries: 0,
                retry_delay: Duration::from_millis(1),
                ..crate::probe::ProbeConfig::default()
            },
        };

        let report = run_verify_live(&opts);
        assert_eq!(report.exit_code(), 0);
        assert_eq!(report.summary.failed, 0);
        assert!(report.stages.remote.ran);
        assert!(report.stages.security.ran);
        assert!(
            report
                .stages
                .remote
                .checks
                .iter()
                .any(|c| c.id == "remote.content_match" && c.passed)
        );
        assert!(
            report
                .stages
                .security
                .checks
                .iter()
                .any(|c| c.id == "security.coop_value" && c.passed)
        );
        assert!(
            report
                .stages
                .security
                .checks
                .iter()
                .any(|c| c.id == "security.coep_value" && c.passed)
        );
    }

    #[test]
    fn run_verify_live_integration_strict_warn_exit_one() {
        let server = TestHttpServer::spawn(false);
        let opts = VerifyLiveOptions {
            url: server.base_url(),
            bundle_path: None,
            security_audit: false,
            strict: true,
            fail_fast: false,
            probe_config: crate::probe::ProbeConfig {
                timeout: Duration::from_secs(2),
                retries: 0,
                retry_delay: Duration::from_millis(1),
                ..crate::probe::ProbeConfig::default()
            },
        };

        let report = run_verify_live(&opts);
        assert_eq!(report.verdict, VerifyVerdict::Warn);
        assert_eq!(report.exit_code(), 1);
        assert!(
            report
                .stages
                .remote
                .checks
                .iter()
                .any(|c| c.id == "remote.coop" && !c.passed)
        );
        assert!(
            report
                .stages
                .remote
                .checks
                .iter()
                .any(|c| c.id == "remote.coep" && !c.passed)
        );
    }

    #[test]
    fn run_verify_live_fail_fast_short_circuits_remote_stage() {
        let dir = tempfile::tempdir().unwrap();
        let bad_bundle = dir.path().join("bundle");
        std::fs::create_dir_all(&bad_bundle).unwrap();
        std::fs::write(bad_bundle.join("index.html"), "<html></html>").unwrap();

        let opts = VerifyLiveOptions {
            url: "http://127.0.0.1:1".to_string(),
            bundle_path: Some(bad_bundle),
            security_audit: true,
            strict: false,
            fail_fast: true,
            probe_config: crate::probe::ProbeConfig {
                timeout: Duration::from_millis(200),
                retries: 0,
                retry_delay: Duration::from_millis(1),
                ..crate::probe::ProbeConfig::default()
            },
        };

        let report = run_verify_live(&opts);
        assert_eq!(report.verdict, VerifyVerdict::Fail);
        assert_eq!(report.exit_code(), 1);
        assert!(report.stages.local.ran);
        assert!(!report.stages.remote.ran);
        assert!(!report.stages.security.ran);
        assert!(report.stages.remote.checks.is_empty());
        assert!(report.stages.security.checks.is_empty());
    }
}
