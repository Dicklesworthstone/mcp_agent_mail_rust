//! `fm-secrets-env-state-jwt-enabled-without-keys` — P0.
//!
//! **Subsystem**: secrets_env_state.
//!
//! ## What's broken
//!
//! `HTTP_JWT_ENABLED=true` is set in the env / config.env, but
//! the keys required by the configured algorithm family are
//! absent or contradict the algorithm list. Concrete shapes:
//!
//! - `HTTP_JWT_ALGORITHMS=HS256` (symmetric) but
//!   `HTTP_JWT_SECRET` is empty/unset.
//! - `HTTP_JWT_ALGORITHMS=RS256,ES256` (asymmetric) but
//!   `HTTP_JWT_JWKS_URL` is empty/unset.
//! - `HTTP_JWT_ALGORITHMS=` empty list with JWT enabled — no
//!   verifier shape to invoke.
//! - `HTTP_JWT_ALGORITHMS=GARBAGE` not in the HS/RS/ES/PS
//!   families — verifier cannot be constructed.
//!
//! Every shape causes `am serve-http` to refuse every request
//! with a 401 (no verifier configured) — clients get a wall of
//! auth failures from a deployment that thinks JWT auth is on
//! but has no keys.
//!
//! ## Detection (pure function)
//!
//! Inputs are the relevant `Config` fields, passed in via
//! `DetectInputs` so tests can construct hermetic scenarios.
//! Production callers build inputs from `Config::from_env()`.
//!
//! Surfaces one finding per Config with a `Vec<JwtProblem>`
//! enumerating every triggering condition.
//!
//! ## Privacy
//!
//! The detector NEVER reads or logs the secret bytes — it only
//! checks `is_empty()`. Findings carry the algorithm list,
//! issuer, audience, and problem categories. Bearer tokens and
//! JWT secrets stay opaque.
//!
//! ## Fix
//!
//! **Detect-only.** A safe auto-fix would flip
//! `HTTP_JWT_ENABLED=false` in config.env (with explicit
//! `--yes`), but doing so silently disables an intended
//! security boundary — operators must consciously decide to
//! drop JWT or to supply keys. The manual_remediation envelope
//! enumerates both directions (set keys OR disable JWT).

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;

pub const FM_ID: &str = "fm-secrets-env-state-jwt-enabled-without-keys";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "secrets_env_state";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum JwtProblem {
    /// HS* algorithm configured but `HTTP_JWT_SECRET` empty/unset.
    HsAlgoWithoutSecret,
    /// RS*/ES*/PS* algorithm configured but `HTTP_JWT_JWKS_URL`
    /// empty/unset.
    AsymmetricAlgoWithoutJwks,
    /// `HTTP_JWT_ALGORITHMS` is empty — no verifier shape.
    NoAlgorithmsConfigured,
    /// `HTTP_JWT_ALGORITHMS` contains values outside the
    /// HS/RS/ES/PS families — verifier cannot be constructed.
    UnknownAlgorithmFamily,
}

impl JwtProblem {
    fn as_kebab(self) -> &'static str {
        match self {
            JwtProblem::HsAlgoWithoutSecret => "hs_algo_without_secret",
            JwtProblem::AsymmetricAlgoWithoutJwks => "asymmetric_algo_without_jwks",
            JwtProblem::NoAlgorithmsConfigured => "no_algorithms_configured",
            JwtProblem::UnknownAlgorithmFamily => "unknown_algorithm_family",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JwtEnabledWithoutKeysFinding {
    pub algorithms: Vec<String>,
    /// Whether `HTTP_JWT_SECRET` is present and non-empty (we
    /// never log the value itself).
    pub has_secret: bool,
    /// Whether `HTTP_JWT_JWKS_URL` is present and non-empty.
    pub has_jwks: bool,
    pub issuer: Option<String>,
    pub audience: Option<String>,
    pub problems: Vec<JwtProblem>,
}

impl JwtEnabledWithoutKeysFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "HTTP_JWT_ENABLED=true but verifier is incomplete: {}",
            self.problems
                .iter()
                .map(|p| p.as_kebab())
                .collect::<Vec<_>>()
                .join(", "),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "algorithms": self.algorithms,
                "has_secret": self.has_secret,
                "has_jwks": self.has_jwks,
                "issuer": self.issuer,
                "audience": self.audience,
                "problems": self.problems,
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                // Detect-only. Fix would either supply keys (we
                // can't fabricate them) or disable JWT (we won't
                // silently drop a security boundary).
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        let mut steps =
            vec!["JWT verifier configuration is incomplete. Choose ONE of:".to_string()];
        let needs_secret = self
            .problems
            .iter()
            .any(|p| matches!(p, JwtProblem::HsAlgoWithoutSecret));
        let needs_jwks = self
            .problems
            .iter()
            .any(|p| matches!(p, JwtProblem::AsymmetricAlgoWithoutJwks));
        if needs_secret {
            steps.push(
                "  (a) Set HTTP_JWT_SECRET=<random-bytes> in $XDG_CONFIG_HOME/mcp-agent-mail/config.env \
                 (use a 256-bit random secret for HS256)."
                    .to_string(),
            );
        }
        if needs_jwks {
            steps.push(
                "  (b) Set HTTP_JWT_JWKS_URL=<https://your-idp/.well-known/jwks.json> in \
                 config.env (required for RS/ES/PS asymmetric verification)."
                    .to_string(),
            );
        }
        if self
            .problems
            .iter()
            .any(|p| matches!(p, JwtProblem::NoAlgorithmsConfigured))
        {
            steps.push(
                "  (c) Set HTTP_JWT_ALGORITHMS=HS256 (or your preferred algorithm) in config.env."
                    .to_string(),
            );
        }
        if self
            .problems
            .iter()
            .any(|p| matches!(p, JwtProblem::UnknownAlgorithmFamily))
        {
            steps.push(format!(
                "  (d) HTTP_JWT_ALGORITHMS={:?} contains entries outside HS/RS/ES/PS families. \
                 Replace with supported algorithm(s) such as HS256, RS256, ES256, or PS256.",
                self.algorithms,
            ));
        }
        steps.push("  -- OR --".to_string());
        steps.push(
            "  (z) Set HTTP_JWT_ENABLED=false in config.env if JWT auth was unintentional. \
             `am doctor` will NOT auto-disable JWT for you because that silently drops a \
             security boundary."
                .to_string(),
        );
        steps.join("\n")
    }
}

#[derive(Debug, Clone)]
pub struct DetectInputs {
    pub http_jwt_enabled: bool,
    pub http_jwt_algorithms: Vec<String>,
    /// `is_some()` indicates the value is set; the actual bytes
    /// are NEVER read by the detector — only its presence.
    pub http_jwt_secret_present: bool,
    pub http_jwt_jwks_url_present: bool,
    pub http_jwt_issuer: Option<String>,
    pub http_jwt_audience: Option<String>,
}

/// Detector. PURE.
pub fn detect(inputs: &DetectInputs) -> Vec<JwtEnabledWithoutKeysFinding> {
    if !inputs.http_jwt_enabled {
        return Vec::new();
    }
    let algos = &inputs.http_jwt_algorithms;
    let needs_secret = algos.iter().any(|a| a.starts_with("HS"));
    let needs_jwks = algos
        .iter()
        .any(|a| a.starts_with("RS") || a.starts_with("ES") || a.starts_with("PS"));
    let known_family = algos.iter().all(|a| {
        a.starts_with("HS") || a.starts_with("RS") || a.starts_with("ES") || a.starts_with("PS")
    });
    let mut problems = Vec::new();
    if algos.is_empty() {
        problems.push(JwtProblem::NoAlgorithmsConfigured);
    } else {
        if needs_secret && !inputs.http_jwt_secret_present {
            problems.push(JwtProblem::HsAlgoWithoutSecret);
        }
        if needs_jwks && !inputs.http_jwt_jwks_url_present {
            problems.push(JwtProblem::AsymmetricAlgoWithoutJwks);
        }
        if !known_family {
            problems.push(JwtProblem::UnknownAlgorithmFamily);
        }
    }
    if problems.is_empty() {
        return Vec::new();
    }
    vec![JwtEnabledWithoutKeysFinding {
        algorithms: algos.clone(),
        has_secret: inputs.http_jwt_secret_present,
        has_jwks: inputs.http_jwt_jwks_url_present,
        issuer: inputs.http_jwt_issuer.clone(),
        audience: inputs.http_jwt_audience.clone(),
        problems,
    }]
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &JwtEnabledWithoutKeysFinding,
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

    fn base() -> DetectInputs {
        DetectInputs {
            http_jwt_enabled: true,
            http_jwt_algorithms: vec!["HS256".to_string()],
            http_jwt_secret_present: true,
            http_jwt_jwks_url_present: false,
            http_jwt_issuer: None,
            http_jwt_audience: None,
        }
    }

    #[test]
    fn detector_returns_empty_when_jwt_disabled() {
        let mut inputs = base();
        inputs.http_jwt_enabled = false;
        inputs.http_jwt_algorithms = Vec::new();
        inputs.http_jwt_secret_present = false;
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn detector_returns_empty_for_healthy_hs256_config() {
        // HS256 + secret present → healthy.
        assert!(detect(&base()).is_empty());
    }

    #[test]
    fn detector_flags_hs_algo_without_secret() {
        let mut inputs = base();
        inputs.http_jwt_secret_present = false;
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .problems
                .contains(&JwtProblem::HsAlgoWithoutSecret)
        );
    }

    #[test]
    fn detector_flags_rs_algo_without_jwks() {
        let mut inputs = base();
        inputs.http_jwt_algorithms = vec!["RS256".to_string()];
        // secret present is irrelevant for RS — JWKS required.
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .problems
                .contains(&JwtProblem::AsymmetricAlgoWithoutJwks)
        );
    }

    #[test]
    fn detector_returns_empty_for_healthy_rs_config_with_jwks() {
        let mut inputs = base();
        inputs.http_jwt_algorithms = vec!["RS256".to_string()];
        inputs.http_jwt_secret_present = false;
        inputs.http_jwt_jwks_url_present = true;
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn detector_flags_empty_algorithms() {
        let mut inputs = base();
        inputs.http_jwt_algorithms = Vec::new();
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .problems
                .contains(&JwtProblem::NoAlgorithmsConfigured)
        );
    }

    #[test]
    fn detector_flags_unknown_algorithm_family() {
        let mut inputs = base();
        inputs.http_jwt_algorithms = vec!["GARBAGE".to_string()];
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .problems
                .contains(&JwtProblem::UnknownAlgorithmFamily)
        );
    }

    #[test]
    fn detector_flags_mixed_hs_and_rs_with_only_secret() {
        // Both families configured; only HS secret present → RS
        // half is broken.
        let mut inputs = base();
        inputs.http_jwt_algorithms = vec!["HS256".to_string(), "RS256".to_string()];
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .problems
                .contains(&JwtProblem::AsymmetricAlgoWithoutJwks)
        );
        assert!(
            !findings[0]
                .problems
                .contains(&JwtProblem::HsAlgoWithoutSecret)
        );
    }

    #[test]
    fn detector_evidence_redacts_secret_bytes() {
        let mut inputs = base();
        inputs.http_jwt_secret_present = false;
        let g = detect(&inputs)[0].to_finding();
        let s = serde_json::to_string(&g).unwrap();
        // Evidence carries `has_secret: bool`, NOT the secret
        // value itself.
        assert!(s.contains("\"has_secret\":false"));
        assert!(!s.contains("HTTP_JWT_SECRET=")); // never include raw env var
    }

    #[test]
    fn finding_severity_is_p0_detect_only() {
        let mut inputs = base();
        inputs.http_jwt_secret_present = false;
        let g = detect(&inputs)[0].to_finding();
        assert_eq!(g.severity, "P0");
        assert!(!g.remediation.auto_fixable);
    }

    #[test]
    fn manual_remediation_enumerates_both_directions() {
        let mut inputs = base();
        inputs.http_jwt_secret_present = false;
        let text = detect(&inputs)[0].manual_remediation_text();
        assert!(text.contains("HTTP_JWT_SECRET"));
        assert!(text.contains("HTTP_JWT_ENABLED=false"));
    }
}
