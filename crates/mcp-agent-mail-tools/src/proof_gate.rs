//! Optional cryptographic proof gate for agent registration.
//!
//! # Why
//!
//! By default MCP Agent Mail uses a **self-asserted** identity model: any caller
//! may `register_agent` under any (well-formed) name. That is the right trade-off
//! for fast local trusted multi-agent coordination and remains the DEFAULT.
//!
//! Some deployments want a stronger guarantee — that only identities blessed by a
//! trusted issuer may register, bound to a specific permission scope. This module
//! implements that as an **optional, off-by-default** gate. When
//! [`ProofGateConfig::enabled`] is `true`, [`enforce`] requires a signed proof
//! bundle before registration proceeds and **fails closed** on anything
//! suspicious.
//!
//! # Proof bundle
//!
//! The bundle is a JSON string passed as the `registration_proof` argument:
//!
//! ```json
//! {
//!   "claims": {
//!     "identity":     "BlueLake",
//!     "project_key":  "/data/projects/backend",
//!     "program":      "claude-code",
//!     "model":        "opus-4.1",
//!     "capabilities": ["send_message", "fetch_inbox", "file_reservation_paths", "acknowledge_message"],
//!     "issued_at":    1720000000,
//!     "expires_at":   1720000300,
//!     "nonce":        "b3RhLW5vbmNlLTE2Ynl0ZXM="
//!   },
//!   "public_key": "<base64 std, 32 raw bytes>",
//!   "signature":  "<base64 std, 64 raw bytes>"
//! }
//! ```
//!
//! The `signature` is an Ed25519 signature (verified with `verify_strict`, which
//! is constant-time and rejects malleable/small-order inputs) over the
//! [`canonical_message`] serialization of `claims`. The signing `public_key` must
//! be one of the configured [trust anchors](ProofGateConfig::trusted_keys).
//!
//! # Canonical signing bytes
//!
//! To make the proof reproducible from any language, canonicalization is explicit
//! (not dependent on JSON key ordering). See [`canonical_message`]: a
//! domain-separation tag followed by newline-delimited `field=value` lines, with
//! `capabilities` sorted, de-duplicated, and comma-joined.
//!
//! # Extensibility
//!
//! Verification is expressed through the [`ProofVerifier`] trait. The concrete,
//! shipped implementation is [`Ed25519TrustAnchorVerifier`]. An external gate
//! (a configured command/endpoint returning allow/deny) can be added later as an
//! additional `ProofVerifier` implementation without touching the registration
//! call sites — [`enforce`] dispatches through the trait.

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use fastmcp::McpError;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::config::ProofGateConfig;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Domain-separation tag prefixed to every canonical signed message. Bumping the
/// version invalidates every previously issued proof.
const PROOF_DOMAIN: &str = "agent-mail-registration-proof:v1";

/// The concrete facts a registration is asserting, which the proof must bind.
///
/// Borrowed from the live `register_agent` / `create_agent_identity` arguments so
/// the proof is validated against *what is actually about to be written*, not
/// against whatever the bundle claims in isolation.
#[derive(Debug)]
pub struct RegistrationRequest<'a> {
    /// The resolved, normalized agent name that will be registered.
    pub agent_name: &'a str,
    /// The `project_key` argument exactly as supplied by the caller.
    pub project_key: &'a str,
    /// The agent program (e.g. `claude-code`).
    pub program: &'a str,
    /// The underlying model identifier.
    pub model: &'a str,
    /// The capabilities that registration will grant this agent. The proof must
    /// authorize a superset of these.
    pub granted_capabilities: &'a [&'a str],
    /// The raw `registration_proof` argument (`None` when the caller omitted it).
    pub proof: Option<&'a str>,
}

/// Claims carried inside a proof bundle.
///
/// NOTE: the declaration order here is irrelevant to signing — [`canonical_message`]
/// fixes the byte layout independently of serde/JSON ordering.
#[derive(Debug, Deserialize)]
struct ProofClaims {
    identity: String,
    project_key: String,
    program: String,
    model: String,
    #[serde(default)]
    capabilities: Vec<String>,
    issued_at: i64,
    expires_at: i64,
    #[serde(default)]
    nonce: String,
}

/// A parsed proof bundle: claims + the key that signed them + the signature.
#[derive(Debug, Deserialize)]
struct ProofBundle {
    claims: ProofClaims,
    public_key: String,
    signature: String,
}

/// Outcome of a proof verification attempt.
///
/// `Allow` means the proof is authentic, trusted, unexpired, and binds the exact
/// registration being attempted. Every `Deny` carries a distinct, stable machine
/// code (`PROOF_*`) plus a human-actionable message.
enum Verdict {
    Allow,
    Deny {
        code: &'static str,
        message: String,
        detail: serde_json::Value,
    },
}

/// Pluggable proof verifier. The built-in [`Ed25519TrustAnchorVerifier`] is the
/// concrete deliverable; an external command/endpoint verifier can implement this
/// trait later without changing any registration call site.
trait ProofVerifier {
    /// Verify `bundle_json` against `request`, returning an allow/deny verdict.
    ///
    /// Implementations MUST fail closed: any parse failure, ambiguity, or
    /// unverifiable input yields a `Deny`.
    fn verify(
        &self,
        bundle_json: &str,
        request: &RegistrationRequest<'_>,
        now_unix: i64,
    ) -> Verdict;
}

/// Built-in verifier: Ed25519 signature over the canonical claims, checked
/// against a configured trust-anchor allowlist, with freshness, binding, and
/// replay enforcement.
struct Ed25519TrustAnchorVerifier<'cfg> {
    config: &'cfg ProofGateConfig,
}

impl ProofVerifier for Ed25519TrustAnchorVerifier<'_> {
    #[allow(clippy::too_many_lines)]
    fn verify(
        &self,
        bundle_json: &str,
        request: &RegistrationRequest<'_>,
        now_unix: i64,
    ) -> Verdict {
        let cfg = self.config;

        // 1. Parse the bundle.
        let bundle: ProofBundle = match serde_json::from_str(bundle_json) {
            Ok(b) => b,
            Err(e) => {
                return deny(
                    "PROOF_MALFORMED",
                    format!("registration_proof is not a valid proof bundle: {e}"),
                    json!({ "stage": "parse" }),
                );
            }
        };

        // 2. Decode key + signature to fixed-size byte arrays.
        let Some(pk_bytes) = b64_decode_fixed::<32>(&bundle.public_key) else {
            return deny(
                "PROOF_MALFORMED",
                "public_key must be base64 (standard) of exactly 32 bytes",
                json!({ "stage": "decode_public_key" }),
            );
        };
        let Some(sig_bytes) = b64_decode_fixed::<64>(&bundle.signature) else {
            return deny(
                "PROOF_MALFORMED",
                "signature must be base64 (standard) of exactly 64 bytes",
                json!({ "stage": "decode_signature" }),
            );
        };

        // 3. Trust-anchor membership. The public key is not secret, so a plain
        //    byte comparison is acceptable here (no secret-dependent timing).
        let trusted = cfg
            .trusted_keys
            .iter()
            .filter_map(|k| b64_decode_fixed::<32>(k))
            .any(|anchor| anchor == pk_bytes);
        if !trusted {
            return deny(
                "PROOF_UNTRUSTED_KEY",
                "registration_proof was signed by a key that is not a configured trust anchor",
                json!({ "public_key": bundle.public_key }),
            );
        }

        // 4. Signature verification (constant-time, strict). Done BEFORE any
        //    binding/replay check so an attacker cannot probe those surfaces
        //    without possessing a valid signature from a trusted key.
        let Ok(verifying_key) = VerifyingKey::from_bytes(&pk_bytes) else {
            return deny(
                "PROOF_MALFORMED",
                "public_key is not a valid Ed25519 verifying key",
                json!({ "stage": "verifying_key" }),
            );
        };
        let signature = Signature::from_bytes(&sig_bytes);
        let message = canonical_message(&bundle.claims);
        if verifying_key
            .verify_strict(message.as_bytes(), &signature)
            .is_err()
        {
            return deny(
                "PROOF_BAD_SIGNATURE",
                "registration_proof signature did not verify against the trusted key",
                json!({ "public_key": bundle.public_key }),
            );
        }

        // 5. Freshness / validity window.
        let claims = &bundle.claims;
        if claims.expires_at <= claims.issued_at {
            return deny(
                "PROOF_MALFORMED",
                "expires_at must be strictly greater than issued_at",
                json!({ "issued_at": claims.issued_at, "expires_at": claims.expires_at }),
            );
        }
        let max_lifetime = i64::try_from(cfg.max_lifetime_seconds).unwrap_or(i64::MAX);
        let lifetime = claims.expires_at - claims.issued_at;
        if lifetime > max_lifetime {
            return deny(
                "PROOF_LIFETIME_TOO_LONG",
                format!(
                    "proof lifetime {lifetime}s exceeds configured maximum {max_lifetime}s \
                     (AM_REGISTRATION_PROOF_MAX_LIFETIME_SECONDS)"
                ),
                json!({ "lifetime_seconds": lifetime, "max_lifetime_seconds": max_lifetime }),
            );
        }
        let skew = i64::try_from(cfg.clock_skew_seconds).unwrap_or(i64::MAX);
        if claims.issued_at.saturating_sub(skew) > now_unix {
            return deny(
                "PROOF_NOT_YET_VALID",
                "registration_proof issued_at is in the future beyond the allowed clock skew",
                json!({ "issued_at": claims.issued_at, "now": now_unix, "clock_skew_seconds": skew }),
            );
        }
        if now_unix > claims.expires_at.saturating_add(skew) {
            return deny(
                "PROOF_EXPIRED",
                "registration_proof has expired",
                json!({ "expires_at": claims.expires_at, "now": now_unix, "clock_skew_seconds": skew }),
            );
        }

        // 6. Binding: the proof must describe THIS exact registration.
        if !claims
            .identity
            .trim()
            .eq_ignore_ascii_case(request.agent_name.trim())
        {
            return deny(
                "PROOF_IDENTITY_MISMATCH",
                format!(
                    "registration_proof binds identity '{}' but registration is for '{}'. \
                     When the proof gate is enabled you must register the exact name the proof \
                     authorizes (do not omit `name` to auto-generate).",
                    claims.identity, request.agent_name
                ),
                json!({ "proof_identity": claims.identity, "requested_identity": request.agent_name }),
            );
        }
        if claims.project_key.trim() != request.project_key.trim() {
            return deny(
                "PROOF_PROJECT_MISMATCH",
                format!(
                    "registration_proof binds project_key '{}' but registration targets '{}'",
                    claims.project_key, request.project_key
                ),
                json!({ "proof_project_key": claims.project_key, "requested_project_key": request.project_key }),
            );
        }
        if claims.program.trim() != request.program.trim()
            || claims.model.trim() != request.model.trim()
        {
            return deny(
                "PROOF_BINDING_MISMATCH",
                format!(
                    "registration_proof binds program/model '{}'/'{}' but registration uses '{}'/'{}'",
                    claims.program, claims.model, request.program, request.model
                ),
                json!({
                    "proof_program": claims.program,
                    "proof_model": claims.model,
                    "requested_program": request.program,
                    "requested_model": request.model,
                }),
            );
        }

        // 7. Scope: every capability about to be granted must be authorized by
        //    the proof (granted ⊆ proof.capabilities).
        let missing: Vec<&str> = request
            .granted_capabilities
            .iter()
            .copied()
            .filter(|granted| {
                !claims
                    .capabilities
                    .iter()
                    .any(|authorized| authorized.trim() == *granted)
            })
            .collect();
        if !missing.is_empty() {
            return deny(
                "PROOF_SCOPE_MISMATCH",
                format!(
                    "registration would grant capabilities not authorized by the proof: {}",
                    missing.join(", ")
                ),
                json!({
                    "missing_capabilities": missing,
                    "proof_capabilities": claims.capabilities,
                }),
            );
        }

        // 8. Replay: consume the nonce LAST, so a proof that fails any earlier
        //    check never burns its nonce. Only reached once the proof is fully
        //    authentic, fresh, and correctly bound.
        if cfg.require_nonce {
            if claims.nonce.trim().is_empty() {
                return deny(
                    "PROOF_MALFORMED",
                    "nonce is required when AM_REGISTRATION_PROOF_REQUIRE_NONCE is enabled",
                    json!({ "stage": "nonce" }),
                );
            }
            // Keep the consumed nonce recorded until its (skewed) expiry so a
            // replay within the proof's validity window is rejected.
            let retain_until = claims.expires_at.saturating_add(skew);
            if consume_nonce(&pk_bytes, claims.nonce.trim(), retain_until, now_unix).is_err() {
                return deny(
                    "PROOF_REPLAYED_NONCE",
                    "registration_proof nonce has already been used",
                    json!({ "nonce": claims.nonce }),
                );
            }
        }

        Verdict::Allow
    }
}

/// Enforce the registration proof gate for a registration attempt.
///
/// When the gate is disabled (the default), this is a no-op and returns `Ok(())`
/// with zero behavior change. When enabled, it verifies the proof and returns an
/// `Err` (registration must abort, fail-closed) on any problem.
///
/// # Errors
///
/// Returns a `McpError` (tool error payload with a distinct `PROOF_*` type) when
/// the gate is enabled and the proof is missing, malformed, untrusted, has a bad
/// signature, is expired / not-yet-valid / over-long-lived, does not bind the
/// requested identity / project / program / model / capability scope, or replays
/// a previously consumed nonce.
pub fn enforce(request: &RegistrationRequest<'_>) -> Result<(), McpError> {
    let config = Config::get();
    enforce_with_config(&config.proof_gate, request, now_unix_seconds())
}

/// Testable core of [`enforce`]: takes an explicit gate config and clock so unit
/// tests can exercise every branch deterministically.
fn enforce_with_config(
    gate: &ProofGateConfig,
    request: &RegistrationRequest<'_>,
    now_unix: i64,
) -> Result<(), McpError> {
    if !gate.enabled {
        // Self-asserted registration: unchanged behavior, no proof required.
        return Ok(());
    }

    // Proof presence is checked here (not inside the verifier) so a missing proof
    // yields the same fail-closed verdict regardless of which verifier is wired.
    let Some(raw) = request.proof.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(mk_error(
            "PROOF_REQUIRED",
            "The registration proof gate is enabled: a signed `registration_proof` bundle is \
             required to register an agent.",
            json!({ "gate": "registration.proof_gate" }),
        ));
    };

    let verifier = Ed25519TrustAnchorVerifier { config: gate };
    match verifier.verify(raw, request, now_unix) {
        Verdict::Allow => Ok(()),
        Verdict::Deny {
            code,
            message,
            detail,
        } => Err(mk_error(code, message, detail)),
    }
}

/// Build a `Deny` verdict.
fn deny(code: &'static str, message: impl Into<String>, detail: serde_json::Value) -> Verdict {
    Verdict::Deny {
        code,
        message: message.into(),
        detail,
    }
}

/// Build the tool-layer `McpError` for a denied registration. `recoverable` is
/// `false`: these are hard security refusals, not transient conditions.
fn mk_error(code: &str, message: impl Into<String>, detail: serde_json::Value) -> McpError {
    crate::tool_util::legacy_tool_error(code, message, false, detail)
}

/// Deterministic canonical byte serialization of the claims that is signed.
///
/// Reproducible in any language: a domain tag followed by newline-delimited
/// `field=value` lines in a fixed order. `capabilities` are sorted ascending,
/// de-duplicated, and comma-joined so signer and verifier agree regardless of
/// input ordering.
fn canonical_message(claims: &ProofClaims) -> String {
    let mut caps: Vec<String> = claims
        .capabilities
        .iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    caps.sort();
    caps.dedup();
    format!(
        "{PROOF_DOMAIN}\n\
         identity={identity}\n\
         project_key={project_key}\n\
         program={program}\n\
         model={model}\n\
         capabilities={capabilities}\n\
         issued_at={issued_at}\n\
         expires_at={expires_at}\n\
         nonce={nonce}",
        identity = claims.identity,
        project_key = claims.project_key,
        program = claims.program,
        model = claims.model,
        capabilities = caps.join(","),
        issued_at = claims.issued_at,
        expires_at = claims.expires_at,
        nonce = claims.nonce,
    )
}

/// Decode base64 (standard alphabet) into a fixed-size array, or `None` if the
/// input is not valid base64 or has the wrong length.
fn b64_decode_fixed<const N: usize>(input: &str) -> Option<[u8; N]> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .ok()?;
    if bytes.len() != N {
        return None;
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    Some(out)
}

/// Current wall-clock time in whole seconds since the Unix epoch.
fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// Process-wide store of consumed `(public_key, nonce)` pairs → retain-until ts.
fn nonce_store() -> &'static Mutex<HashMap<(Vec<u8>, String), i64>> {
    static STORE: OnceLock<Mutex<HashMap<(Vec<u8>, String), i64>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a nonce as consumed. Returns `Err(())` if it was already consumed
/// (replay). Expired entries are pruned opportunistically to bound memory.
fn consume_nonce(
    public_key: &[u8],
    nonce: &str,
    retain_until: i64,
    now_unix: i64,
) -> Result<(), ()> {
    let store = nonce_store();
    let mut guard = store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|_, expiry| *expiry >= now_unix);
    let key = (public_key.to_vec(), nonce.to_string());
    if guard.contains_key(&key) {
        return Err(());
    }
    guard.insert(key, retain_until);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::Value;

    const CAPS: &[&str] = &[
        "send_message",
        "fetch_inbox",
        "file_reservation_paths",
        "acknowledge_message",
    ];

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn gate_with_anchor(key: &SigningKey) -> ProofGateConfig {
        ProofGateConfig {
            enabled: true,
            trusted_keys: vec![b64(key.verifying_key().as_bytes())],
            max_lifetime_seconds: 300,
            clock_skew_seconds: 60,
            require_nonce: true,
        }
    }

    /// Build a signed bundle JSON. `mutate` can tweak the claims JSON before
    /// signing to exercise mismatch branches, and `resign` controls whether the
    /// signature is recomputed over the (possibly mutated) claims.
    struct ClaimsSpec {
        identity: String,
        project_key: String,
        program: String,
        model: String,
        capabilities: Vec<String>,
        issued_at: i64,
        expires_at: i64,
        nonce: String,
    }

    impl ClaimsSpec {
        fn valid(now: i64) -> Self {
            Self {
                identity: "BlueLake".to_string(),
                project_key: "/data/projects/backend".to_string(),
                program: "claude-code".to_string(),
                model: "opus-4.1".to_string(),
                capabilities: CAPS.iter().map(|s| (*s).to_string()).collect(),
                issued_at: now,
                expires_at: now + 120,
                nonce: format!("nonce-{now}"),
            }
        }

        fn to_claims(&self) -> ProofClaims {
            ProofClaims {
                identity: self.identity.clone(),
                project_key: self.project_key.clone(),
                program: self.program.clone(),
                model: self.model.clone(),
                capabilities: self.capabilities.clone(),
                issued_at: self.issued_at,
                expires_at: self.expires_at,
                nonce: self.nonce.clone(),
            }
        }

        fn signed_bundle(&self, key: &SigningKey) -> String {
            let claims = self.to_claims();
            let sig = key.sign(canonical_message(&claims).as_bytes());
            json!({
                "claims": {
                    "identity": self.identity,
                    "project_key": self.project_key,
                    "program": self.program,
                    "model": self.model,
                    "capabilities": self.capabilities,
                    "issued_at": self.issued_at,
                    "expires_at": self.expires_at,
                    "nonce": self.nonce,
                },
                "public_key": b64(key.verifying_key().as_bytes()),
                "signature": b64(&sig.to_bytes()),
            })
            .to_string()
        }
    }

    fn request<'a>(proof: Option<&'a str>) -> RegistrationRequest<'a> {
        RegistrationRequest {
            agent_name: "BlueLake",
            project_key: "/data/projects/backend",
            program: "claude-code",
            model: "opus-4.1",
            granted_capabilities: CAPS,
            proof,
        }
    }

    fn deny_code(err: &McpError) -> String {
        err.data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|root| root.get("error"))
            .and_then(Value::as_object)
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
            .expect("error payload type")
            .to_string()
    }

    #[test]
    fn disabled_gate_is_noop_even_without_proof() {
        let gate = ProofGateConfig::default();
        assert!(!gate.enabled);
        assert!(enforce_with_config(&gate, &request(None), 1_000).is_ok());
    }

    #[test]
    fn enabled_with_valid_proof_allows() {
        let key = signing_key(1);
        let gate = gate_with_anchor(&key);
        let now = 1_000_000;
        let bundle = ClaimsSpec::valid(now).signed_bundle(&key);
        assert!(enforce_with_config(&gate, &request(Some(&bundle)), now).is_ok());
    }

    #[test]
    fn enabled_missing_proof_fails_closed() {
        let key = signing_key(2);
        let gate = gate_with_anchor(&key);
        let err = enforce_with_config(&gate, &request(None), 1_000).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_REQUIRED");
    }

    #[test]
    fn malformed_proof_fails_closed() {
        let key = signing_key(3);
        let gate = gate_with_anchor(&key);
        let err = enforce_with_config(&gate, &request(Some("{not json")), 1_000).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_MALFORMED");
    }

    #[test]
    fn untrusted_key_fails_closed() {
        let issuer = signing_key(4);
        let attacker = signing_key(5);
        // Trust only the issuer, but sign with the attacker's key.
        let gate = gate_with_anchor(&issuer);
        let now = 2_000_000;
        let bundle = ClaimsSpec::valid(now).signed_bundle(&attacker);
        let err = enforce_with_config(&gate, &request(Some(&bundle)), now).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_UNTRUSTED_KEY");
    }

    #[test]
    fn bad_signature_fails_closed() {
        let key = signing_key(6);
        let gate = gate_with_anchor(&key);
        let now = 3_000_000;
        let mut bundle: Value =
            serde_json::from_str(&ClaimsSpec::valid(now).signed_bundle(&key)).unwrap();
        // Corrupt one claim without re-signing → signature no longer matches.
        bundle["claims"]["model"] = Value::from("gpt-5.5");
        let tampered = bundle.to_string();
        // The request must still match the (tampered) claims so we reach the
        // signature check rather than a binding check; align request.model.
        let req = RegistrationRequest {
            model: "gpt-5.5",
            ..request(Some(&tampered))
        };
        let err = enforce_with_config(&gate, &req, now).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_BAD_SIGNATURE");
    }

    #[test]
    fn expired_proof_fails_closed() {
        let key = signing_key(7);
        let gate = gate_with_anchor(&key);
        let now = 4_000_000;
        let bundle = ClaimsSpec::valid(now).signed_bundle(&key);
        // Evaluate far past expiry (beyond skew).
        let err = enforce_with_config(&gate, &request(Some(&bundle)), now + 10_000).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_EXPIRED");
    }

    #[test]
    fn not_yet_valid_proof_fails_closed() {
        let key = signing_key(8);
        let gate = gate_with_anchor(&key);
        let issued = 5_000_000;
        let bundle = ClaimsSpec::valid(issued).signed_bundle(&key);
        // Evaluate well before issued_at (beyond skew).
        let err = enforce_with_config(&gate, &request(Some(&bundle)), issued - 10_000).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_NOT_YET_VALID");
    }

    #[test]
    fn lifetime_too_long_fails_closed() {
        let key = signing_key(9);
        let gate = gate_with_anchor(&key);
        let now = 6_000_000;
        let mut spec = ClaimsSpec::valid(now);
        spec.expires_at = now + 100_000; // exceeds max_lifetime_seconds (300)
        let bundle = spec.signed_bundle(&key);
        let err = enforce_with_config(&gate, &request(Some(&bundle)), now).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_LIFETIME_TOO_LONG");
    }

    #[test]
    fn identity_mismatch_fails_closed() {
        let key = signing_key(10);
        let gate = gate_with_anchor(&key);
        let now = 7_000_000;
        let mut spec = ClaimsSpec::valid(now);
        spec.identity = "RedStone".to_string();
        let bundle = spec.signed_bundle(&key);
        // request still asks for BlueLake.
        let err = enforce_with_config(&gate, &request(Some(&bundle)), now).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_IDENTITY_MISMATCH");
    }

    #[test]
    fn project_mismatch_fails_closed() {
        let key = signing_key(11);
        let gate = gate_with_anchor(&key);
        let now = 8_000_000;
        let mut spec = ClaimsSpec::valid(now);
        spec.project_key = "/data/projects/other".to_string();
        let bundle = spec.signed_bundle(&key);
        let err = enforce_with_config(&gate, &request(Some(&bundle)), now).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_PROJECT_MISMATCH");
    }

    #[test]
    fn program_model_binding_mismatch_fails_closed() {
        let key = signing_key(12);
        let gate = gate_with_anchor(&key);
        let now = 9_000_000;
        let mut spec = ClaimsSpec::valid(now);
        spec.program = "codex-cli".to_string();
        let bundle = spec.signed_bundle(&key);
        // request.program is "claude-code" → mismatch, but signature is valid.
        let err = enforce_with_config(&gate, &request(Some(&bundle)), now).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_BINDING_MISMATCH");
    }

    #[test]
    fn scope_mismatch_fails_closed() {
        let key = signing_key(13);
        let gate = gate_with_anchor(&key);
        let now = 10_000_000;
        let mut spec = ClaimsSpec::valid(now);
        // Proof authorizes fewer capabilities than will be granted.
        spec.capabilities = vec!["fetch_inbox".to_string()];
        let bundle = spec.signed_bundle(&key);
        let err = enforce_with_config(&gate, &request(Some(&bundle)), now).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_SCOPE_MISMATCH");
    }

    #[test]
    fn replayed_nonce_fails_closed() {
        let key = signing_key(14);
        let gate = gate_with_anchor(&key);
        let now = 11_000_000;
        // Unique nonce/key per test run to avoid cross-test interference in the
        // process-global nonce store.
        let mut spec = ClaimsSpec::valid(now);
        spec.nonce = "replay-fixed-nonce".to_string();
        let bundle = spec.signed_bundle(&key);
        // First use succeeds and consumes the nonce.
        assert!(enforce_with_config(&gate, &request(Some(&bundle)), now).is_ok());
        // Second identical use is a replay.
        let err = enforce_with_config(&gate, &request(Some(&bundle)), now).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_REPLAYED_NONCE");
    }

    #[test]
    fn empty_trust_anchor_list_fails_closed() {
        let key = signing_key(15);
        let mut gate = gate_with_anchor(&key);
        gate.trusted_keys.clear(); // enabled but no anchors → nothing can register
        let now = 12_000_000;
        let bundle = ClaimsSpec::valid(now).signed_bundle(&key);
        let err = enforce_with_config(&gate, &request(Some(&bundle)), now).unwrap_err();
        assert_eq!(deny_code(&err), "PROOF_UNTRUSTED_KEY");
    }

    #[test]
    fn nonce_optional_when_replay_tracking_disabled() {
        let key = signing_key(16);
        let mut gate = gate_with_anchor(&key);
        gate.require_nonce = false;
        let now = 13_000_000;
        let mut spec = ClaimsSpec::valid(now);
        spec.nonce = String::new();
        let bundle = spec.signed_bundle(&key);
        // Same bundle can be used twice when nonce tracking is off.
        assert!(enforce_with_config(&gate, &request(Some(&bundle)), now).is_ok());
        assert!(enforce_with_config(&gate, &request(Some(&bundle)), now).is_ok());
    }
}
