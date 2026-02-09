//! Cryptographic operations for bundle signing and encryption.
//!
//! - Ed25519 manifest signing via `ed25519-dalek`
//! - Age encryption/decryption via CLI shelling

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ShareError, ShareResult};

/// Signature metadata written to `manifest.sig.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestSignature {
    pub algorithm: String,
    pub signature: String,
    pub manifest_sha256: String,
    pub public_key: String,
    pub generated_at: String,
}

/// Sign a manifest.json with an Ed25519 key.
///
/// `signing_key_path` should contain a 32-byte Ed25519 seed (or 64-byte expanded
/// key — only first 32 bytes are used).
///
/// Returns the signature metadata which is also written to `output_path`.
pub fn sign_manifest(
    manifest_path: &Path,
    signing_key_path: &Path,
    output_path: &Path,
    overwrite: bool,
) -> ShareResult<ManifestSignature> {
    use ed25519_dalek::{Signer, SigningKey};

    if !manifest_path.exists() {
        return Err(ShareError::ManifestNotFound {
            path: manifest_path.display().to_string(),
        });
    }

    if output_path.exists() && !overwrite {
        return Err(ShareError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("signature file already exists: {}", output_path.display()),
        )));
    }

    // Read signing key (32-byte seed or 64-byte expanded — use first 32)
    let key_bytes = std::fs::read(signing_key_path)?;
    if key_bytes.len() != 32 && key_bytes.len() != 64 {
        return Err(ShareError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "signing key must be 32 or 64 bytes, got {}",
                key_bytes.len()
            ),
        )));
    }

    let seed: [u8; 32] = key_bytes[..32]
        .try_into()
        .expect("slice is exactly 32 bytes");
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = signing_key.verifying_key();

    // Read and hash manifest
    let manifest_bytes = std::fs::read(manifest_path)?;
    let manifest_sha256 = hex_sha256(&manifest_bytes);

    // Sign
    let signature = signing_key.sign(&manifest_bytes);

    let sig_meta = ManifestSignature {
        algorithm: "ed25519".to_string(),
        signature: base64_encode(signature.to_bytes().as_slice()),
        manifest_sha256,
        public_key: base64_encode(verifying_key.as_bytes()),
        generated_at: chrono::Utc::now().to_rfc3339(),
    };

    // Write signature file
    let json = serde_json::to_string_pretty(&sig_meta).map_err(|e| ShareError::ManifestParse {
        message: e.to_string(),
    })?;
    std::fs::write(output_path, json)?;

    Ok(sig_meta)
}

/// Verify SRI hashes and Ed25519 signature for a bundle.
///
/// Returns verification results.
pub fn verify_bundle(
    bundle_root: &Path,
    public_key_b64: Option<&str>,
) -> ShareResult<VerifyResult> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let manifest_path = bundle_root.join("manifest.json");
    if !manifest_path.exists() {
        return Err(ShareError::ManifestNotFound {
            path: bundle_root.display().to_string(),
        });
    }

    let manifest_bytes = std::fs::read(&manifest_path)?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).map_err(|e| ShareError::ManifestParse {
            message: e.to_string(),
        })?;

    // Check SRI hashes
    let mut sri_checked = false;
    if let Some(viewer) = manifest.get("viewer") {
        if let Some(sri_map) = viewer.get("sri").and_then(|v| v.as_object()) {
            sri_checked = true;
            for (relative_path, expected_sri) in sri_map {
                if let Some(expected) = expected_sri.as_str() {
                    let file_path = bundle_root.join(relative_path);
                    if file_path.exists() {
                        let content = std::fs::read(&file_path)?;
                        let actual_hash =
                            format!("sha256-{}", base64_encode(&sha256_bytes(&content)));
                        if actual_hash != expected {
                            return Ok(VerifyResult {
                                bundle: bundle_root.display().to_string(),
                                sri_checked: true,
                                sri_valid: false,
                                signature_checked: false,
                                signature_verified: false,
                                key_source: None,
                                error: Some(format!(
                                    "SRI mismatch for {relative_path}: expected {expected}, got {actual_hash}"
                                )),
                            });
                        }
                    } else {
                        return Ok(VerifyResult {
                            bundle: bundle_root.display().to_string(),
                            sri_checked: true,
                            sri_valid: false,
                            signature_checked: false,
                            signature_verified: false,
                            key_source: None,
                            error: Some(format!("SRI-referenced file missing: {relative_path}")),
                        });
                    }
                }
            }
        }
    }

    // Check Ed25519 signature (requires sig file to exist)
    let sig_path = bundle_root.join("manifest.sig.json");
    let mut signature_checked = false;
    let mut signature_verified = false;
    let mut key_source: Option<String> = None;

    if sig_path.exists() {
        signature_checked = true;

        let sig_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sig_path)?).map_err(|e| {
                ShareError::ManifestParse {
                    message: e.to_string(),
                }
            })?;

        // Explicit public key takes precedence over the one embedded in the sig file.
        // NOTE: When falling back to the embedded key, verification only proves internal
        // consistency (the manifest matches *some* key), not authenticity. An attacker
        // can re-sign with their own key. Callers requiring trust should pass an explicit
        // public_key_b64.
        let (pub_key_str, ks) = if let Some(explicit) = public_key_b64 {
            (Some(explicit.to_string()), Some("explicit".to_string()))
        } else {
            let embedded = sig_json
                .get("public_key")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let source = embedded.as_ref().map(|_| "embedded".to_string());
            (embedded, source)
        };
        key_source = ks;

        let sig_str = sig_json.get("signature").and_then(|v| v.as_str());

        if let (Some(pk_b64), Some(sig_b64)) = (pub_key_str, sig_str) {
            if let (Ok(pk_bytes), Ok(sig_bytes)) = (base64_decode(&pk_b64), base64_decode(sig_b64))
            {
                if pk_bytes.len() == 32 && sig_bytes.len() == 64 {
                    let pk: [u8; 32] = pk_bytes.try_into().unwrap();
                    let sig: [u8; 64] = sig_bytes.try_into().unwrap();
                    if let Ok(verifying_key) = VerifyingKey::from_bytes(&pk) {
                        let signature = Signature::from_bytes(&sig);
                        signature_verified =
                            verifying_key.verify(&manifest_bytes, &signature).is_ok();
                    }
                }
            }
        }
    }

    Ok(VerifyResult {
        bundle: bundle_root.display().to_string(),
        sri_checked,
        sri_valid: sri_checked,
        signature_checked,
        signature_verified,
        key_source,
        error: None,
    })
}

/// Result of bundle verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResult {
    pub bundle: String,
    pub sri_checked: bool,
    pub sri_valid: bool,
    pub signature_checked: bool,
    pub signature_verified: bool,
    /// Where the public key came from: `"explicit"` (caller-provided),
    /// `"embedded"` (from sig file itself — self-signed, no trust anchor), or `null`.
    pub key_source: Option<String>,
    pub error: Option<String>,
}

/// Encrypt a file using the `age` CLI.
///
/// Returns the encrypted file path (`<input>.age`).
pub fn encrypt_with_age(input: &Path, recipients: &[String]) -> ShareResult<std::path::PathBuf> {
    if recipients.is_empty() {
        return Err(ShareError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "at least one age recipient required",
        )));
    }

    check_age_available()?;

    let output = input.with_extension(
        input
            .extension()
            .map(|e| format!("{}.age", e.to_string_lossy()))
            .unwrap_or_else(|| "age".to_string()),
    );

    let mut cmd = std::process::Command::new("age");
    for r in recipients {
        cmd.arg("-r").arg(r);
    }
    cmd.arg("-o").arg(&output).arg(input);

    let result = cmd.output()?;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(ShareError::Io(std::io::Error::other(format!(
            "age encryption failed: {stderr}"
        ))));
    }

    Ok(output)
}

/// Decrypt an age-encrypted file.
///
/// Provide either `identity` (path to age identity file) or `passphrase`.
pub fn decrypt_with_age(
    encrypted_path: &Path,
    output_path: &Path,
    identity: Option<&Path>,
    passphrase: Option<&str>,
) -> ShareResult<()> {
    // Legacy parity: identity and passphrase are mutually exclusive, and at least one is required.
    if identity.is_some() && passphrase.is_some() {
        return Err(ShareError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "passphrase cannot be combined with identity file",
        )));
    }
    if identity.is_none() && passphrase.is_none() {
        return Err(ShareError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "either identity or passphrase required for decryption",
        )));
    }

    check_age_available()?;

    let mut cmd = std::process::Command::new("age");
    cmd.arg("-d");

    if let Some(id_path) = identity {
        cmd.arg("-i").arg(id_path);
    } else if let Some(_pass) = passphrase {
        // age reads passphrase from stdin when -p is used
        cmd.arg("-p");
    } else {
        // Unreachable because we validated inputs above, but keep a defensive branch.
        return Err(ShareError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "either identity or passphrase required for decryption",
        )));
    }

    cmd.arg("-o").arg(output_path).arg(encrypted_path);

    if let Some(pass) = passphrase {
        use std::io::Write;
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(pass.as_bytes())?;
            stdin.write_all(b"\n")?;
        }
        let status = child.wait()?;
        if !status.success() {
            return Err(ShareError::Io(std::io::Error::other(
                "age decryption failed",
            )));
        }
    } else {
        let result = cmd.output()?;
        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(ShareError::Io(std::io::Error::other(format!(
                "age decryption failed: {stderr}"
            ))));
        }
    }

    Ok(())
}

fn check_age_available() -> ShareResult<()> {
    match std::process::Command::new("age").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err(ShareError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "age CLI not found in PATH. Install from https://github.com/FiloSottile/age",
        ))),
    }
}

fn hex_sha256(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hex::encode(hash)
}

fn sha256_bytes(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn base64_decode(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn try_generate_age_identity(dir: &std::path::Path) -> Option<(std::path::PathBuf, String)> {
        let identity_path = dir.join("age_identity.txt");
        let output = std::process::Command::new("age-keygen")
            .arg("-o")
            .arg(&identity_path)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let recipient = combined
            .lines()
            .find(|line| line.contains("public key:"))
            .and_then(|line| line.split_whitespace().last())
            .map(|s| s.to_string())?;
        Some((identity_path, recipient))
    }

    #[test]
    fn hex_sha256_known_value() {
        let hash = hex_sha256(b"hello");
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn base64_roundtrip() {
        let data = b"test data";
        let encoded = base64_encode(data);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    fn test_key_bytes() -> [u8; 32] {
        [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32,
        ]
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let dir = tempfile::tempdir().unwrap();

        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"test": true}"#).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();
        assert_eq!(sig.algorithm, "ed25519");
        assert!(sig_path.exists());

        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.signature_checked);
        assert!(result.signature_verified);
    }

    #[test]
    fn tampered_manifest_fails_verification() {
        let dir = tempfile::tempdir().unwrap();

        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"test": true}"#).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        // Tamper with the manifest
        std::fs::write(&manifest_path, r#"{"test": false, "tampered": true}"#).unwrap();

        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.signature_checked);
        assert!(
            !result.signature_verified,
            "tampered manifest should fail verification"
        );
    }

    #[test]
    fn sign_refuses_overwrite_without_flag() {
        let dir = tempfile::tempdir().unwrap();

        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"test": true}"#).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        // Second sign without overwrite should fail
        let result = sign_manifest(&manifest_path, &key_path, &sig_path, false);
        assert!(result.is_err());

        // With overwrite should succeed
        let result = sign_manifest(&manifest_path, &key_path, &sig_path, true);
        assert!(result.is_ok());
    }

    #[test]
    fn sign_missing_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let result = sign_manifest(
            &dir.path().join("nonexistent.json"),
            &key_path,
            &dir.path().join("sig.json"),
            false,
        );
        assert!(matches!(result, Err(ShareError::ManifestNotFound { .. })));
    }

    #[test]
    fn sign_short_key_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"test": true}"#).unwrap();

        let key_path = dir.path().join("short.key");
        std::fs::write(&key_path, [1u8; 16]).unwrap(); // Too short

        let result = sign_manifest(
            &manifest_path,
            &key_path,
            &dir.path().join("sig.json"),
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn verify_missing_bundle_errors() {
        let result = verify_bundle(Path::new("/nonexistent"), None);
        assert!(matches!(result, Err(ShareError::ManifestNotFound { .. })));
    }

    #[test]
    fn verify_no_signature_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"test": true}"#).unwrap();

        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(!result.signature_checked);
        assert!(!result.signature_verified);
        assert!(!result.sri_checked);
    }

    #[test]
    fn age_encrypt_decrypt_roundtrip() {
        if check_age_available().is_err() {
            eprintln!("Skipping: age CLI not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
            eprintln!("Skipping: age-keygen not available");
            return;
        };

        let input = dir.path().join("bundle.zip");
        std::fs::write(&input, b"test bundle data").unwrap();

        let encrypted = encrypt_with_age(&input, &[recipient]).unwrap();
        let output = dir.path().join("bundle.decrypted.zip");
        decrypt_with_age(&encrypted, &output, Some(&identity_path), None).unwrap();

        let original = std::fs::read(&input).unwrap();
        let decrypted = std::fs::read(&output).unwrap();
        assert_eq!(original, decrypted);
    }
}
