//! Cryptographic operations for bundle signing and encryption.
//!
//! - Ed25519 manifest signing via `ed25519-dalek`
//! - Age encryption/decryption via CLI shelling

use std::path::{Path, PathBuf};

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

    let meta = std::fs::metadata(&manifest_path)?;
    if meta.len() > 10 * 1024 * 1024 {
        return Err(ShareError::ManifestParse {
            message: "manifest.json too large (>10MB)".to_string(),
        });
    }

    let manifest_bytes = std::fs::read(&manifest_path)?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).map_err(|e| ShareError::ManifestParse {
            message: e.to_string(),
        })?;

    // Check SRI hashes
    let mut sri_checked = false;
    if let Some(viewer) = manifest.get("viewer")
        && let Some(sri_map) = viewer.get("sri").and_then(|v| v.as_object())
    {
        sri_checked = true;
        for (relative_path, expected_sri) in sri_map {
            if let Some(expected) = expected_sri.as_str() {
                let file_path = resolve_sri_file_path(bundle_root, relative_path);
                if file_path.exists() {
                    let content = std::fs::read(&file_path)?;
                    let actual_hash = format!("sha256-{}", base64_encode(&sha256_bytes(&content)));
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

        if let (Some(pk_b64), Some(sig_b64)) = (pub_key_str, sig_str)
            && let (Ok(pk_bytes), Ok(sig_bytes)) = (base64_decode(&pk_b64), base64_decode(sig_b64))
            && pk_bytes.len() == 32
            && sig_bytes.len() == 64
        {
            let pk: [u8; 32] = pk_bytes.try_into().unwrap();
            let sig: [u8; 64] = sig_bytes.try_into().unwrap();
            if let Ok(verifying_key) = VerifyingKey::from_bytes(&pk) {
                let signature = Signature::from_bytes(&sig);
                signature_verified = verifying_key.verify(&manifest_bytes, &signature).is_ok();
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

fn resolve_sri_file_path(bundle_root: &Path, relative_path: &str) -> PathBuf {
    // Historical manifests store SRI paths relative to `viewer/` (e.g. `vendor/foo.js`),
    // while some tooling may emit bundle-root relative paths. Accept either.
    let direct = bundle_root.join(relative_path);
    if direct.exists() {
        return direct;
    }
    bundle_root.join("viewer").join(relative_path)
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

    fn extract_zip_archive(zip_path: &Path, output_dir: &Path) {
        let file = std::fs::File::open(zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let output = output_dir.join(entry.name());
            if entry.is_dir() {
                std::fs::create_dir_all(&output).unwrap();
                continue;
            }
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            let mut out_file = std::fs::File::create(&output).unwrap();
            std::io::copy(&mut entry, &mut out_file).unwrap();
        }
    }

    fn write_signed_bundle_fixture(bundle_dir: &Path, bundle_type: &str) -> String {
        let db_bytes = format!("mailbox-data-{bundle_type}").into_bytes();
        std::fs::write(bundle_dir.join("mailbox.sqlite3"), &db_bytes).unwrap();

        let mut sri_entries = serde_json::Map::new();
        let viewer_files: &[(&str, &[u8])] = if bundle_type == "full" {
            &[
                ("vendor/app.js", b"console.log('full');"),
                ("vendor/style.css", b"body{margin:0;}"),
            ]
        } else {
            &[("vendor/incremental.js", b"console.log('incremental');")]
        };
        for (relative, content) in viewer_files {
            let path = bundle_dir.join("viewer").join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, content).unwrap();
            let sri = format!("sha256-{}", base64_encode(&sha256_bytes(content)));
            sri_entries.insert((*relative).to_string(), serde_json::Value::String(sri));
        }

        let manifest = serde_json::json!({
            "schema_version": "0.1.0",
            "bundle_type": bundle_type,
            "database": {
                "path": "mailbox.sqlite3",
                "size_bytes": db_bytes.len(),
                "sha256": hex_sha256(&db_bytes),
            },
            "viewer": {
                "sri": sri_entries,
            },
        });
        let manifest_path = bundle_dir.join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let key_path = bundle_dir.join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();
        let sig_path = bundle_dir.join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();
        sig.public_key
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
        // Without explicit public key, falls back to embedded key
        assert_eq!(result.key_source.as_deref(), Some("embedded"));
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
        assert!(result.key_source.is_none());
    }

    #[test]
    fn verify_sri_paths_resolve_from_viewer_directory() {
        let dir = tempfile::tempdir().unwrap();
        let vendor_dir = dir.path().join("viewer").join("vendor");
        std::fs::create_dir_all(&vendor_dir).unwrap();
        let css_path = vendor_dir.join("clusterize.min.css");
        std::fs::write(&css_path, b".clusterize{display:block}").unwrap();

        let expected_sri = format!(
            "sha256-{}",
            base64_encode(&sha256_bytes(b".clusterize{display:block}"))
        );
        let manifest = serde_json::json!({
            "viewer": {
                "sri": {
                    "vendor/clusterize.min.css": expected_sri
                }
            }
        });
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();

        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.sri_checked);
        assert!(result.sri_valid);
        assert!(result.error.is_none());
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

    // =====================================================================
    // NEW: Encryption roundtrip tests (br-3h13.6.1)
    // =====================================================================

    // --- Ed25519 sign/verify roundtrip with different key sizes ---

    #[test]
    fn sign_verify_roundtrip_32_byte_key() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"bundle_type":"full"}"#).unwrap();

        let key_path = dir.path().join("key32.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        // Verify with explicit public key
        let result = verify_bundle(dir.path(), Some(&sig.public_key)).unwrap();
        assert!(result.signature_checked);
        assert!(result.signature_verified);
        assert_eq!(result.key_source.as_deref(), Some("explicit"));
    }

    #[test]
    fn sign_verify_roundtrip_64_byte_key() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"bundle_type":"incremental"}"#).unwrap();

        // 64-byte expanded key (only first 32 bytes used as seed)
        let mut key64 = [0u8; 64];
        for (i, byte) in key64.iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        let key_path = dir.path().join("key64.key");
        std::fs::write(&key_path, key64).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        let result = verify_bundle(dir.path(), Some(&sig.public_key)).unwrap();
        assert!(result.signature_checked);
        assert!(result.signature_verified);
    }

    // --- Wrong key decryption failure ---

    #[test]
    fn verify_with_wrong_public_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"test": "wrong key"}"#).unwrap();

        // Sign with key A
        let key_a = dir.path().join("key_a.key");
        std::fs::write(&key_a, test_key_bytes()).unwrap();
        let sig_path = dir.path().join("manifest.sig.json");
        sign_manifest(&manifest_path, &key_a, &sig_path, false).unwrap();

        // Create key B (different seed)
        let key_b_seed: [u8; 32] = [
            99, 98, 97, 96, 95, 94, 93, 92, 91, 90, 89, 88, 87, 86, 85, 84, 83, 82, 81, 80, 79, 78,
            77, 76, 75, 74, 73, 72, 71, 70, 69, 68,
        ];
        let key_b_signing = ed25519_dalek::SigningKey::from_bytes(&key_b_seed);
        let key_b_pub = base64_encode(key_b_signing.verifying_key().as_bytes());

        // Verify with wrong public key should fail
        let result = verify_bundle(dir.path(), Some(&key_b_pub)).unwrap();
        assert!(result.signature_checked);
        assert!(
            !result.signature_verified,
            "verification with wrong key should fail"
        );
    }

    // --- Corrupted ciphertext / signature detection ---

    #[test]
    fn corrupted_signature_bytes_fails_verification() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"data": "integrity check"}"#).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        // Corrupt the signature by flipping bits
        let sig_bytes = base64_decode(&sig.signature).unwrap();
        let mut corrupted = sig_bytes.clone();
        corrupted[0] ^= 0xFF;
        corrupted[31] ^= 0xAA;

        // Write corrupted sig file
        let corrupted_sig = ManifestSignature {
            signature: base64_encode(&corrupted),
            ..sig
        };
        let json = serde_json::to_string_pretty(&corrupted_sig).unwrap();
        std::fs::write(&sig_path, json).unwrap();

        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.signature_checked);
        assert!(
            !result.signature_verified,
            "corrupted signature should fail verification"
        );
    }

    #[test]
    fn truncated_signature_fails_verification() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"data": "truncated sig"}"#).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        // Truncate signature to 32 bytes (should be 64)
        let sig_bytes = base64_decode(&sig.signature).unwrap();
        let truncated = base64_encode(&sig_bytes[..32]);

        let truncated_sig = serde_json::json!({
            "algorithm": "ed25519",
            "signature": truncated,
            "manifest_sha256": sig.manifest_sha256,
            "public_key": sig.public_key,
            "generated_at": sig.generated_at,
        });
        std::fs::write(
            &sig_path,
            serde_json::to_string_pretty(&truncated_sig).unwrap(),
        )
        .unwrap();

        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.signature_checked);
        assert!(
            !result.signature_verified,
            "truncated signature should fail verification"
        );
    }

    // --- Empty plaintext / manifest roundtrip ---

    #[test]
    fn sign_verify_empty_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        // Empty JSON object is a valid manifest
        std::fs::write(&manifest_path, "{}").unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();
        assert_eq!(sig.algorithm, "ed25519");

        let result = verify_bundle(dir.path(), Some(&sig.public_key)).unwrap();
        assert!(result.signature_checked);
        assert!(result.signature_verified);
    }

    #[test]
    fn sign_verify_single_byte_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        // Single byte content (not valid JSON, but sign_manifest reads raw bytes)
        std::fs::write(&manifest_path, b"x").unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        // Verification will fail at manifest parse stage, but signing should work
        assert!(!sig.signature.is_empty());
        assert!(!sig.manifest_sha256.is_empty());
    }

    // --- Large payload roundtrip ---

    #[test]
    fn sign_verify_large_manifest() {
        let dir = tempfile::tempdir().unwrap();

        // Generate a 1MB+ JSON manifest
        let mut large_json = String::from(r#"{"data": ""#);
        // Append enough data to exceed 1MB
        let filler = "A".repeat(1_100_000);
        large_json.push_str(&filler);
        large_json.push_str(r#""}"#);
        assert!(large_json.len() > 1_000_000);

        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, &large_json).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        let result = verify_bundle(dir.path(), Some(&sig.public_key)).unwrap();
        assert!(result.signature_checked);
        assert!(
            result.signature_verified,
            "large manifest should verify correctly"
        );

        // Verify SHA256 hash is correct
        let expected_hash = hex_sha256(large_json.as_bytes());
        assert_eq!(sig.manifest_sha256, expected_hash);
    }

    // --- Key derivation consistency (same seed -> same key pair) ---

    #[test]
    fn same_seed_produces_same_signature() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"determinism": true}"#).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        // Sign twice with the same key and manifest
        let sig_path1 = dir.path().join("sig1.json");
        let sig1 = sign_manifest(&manifest_path, &key_path, &sig_path1, false).unwrap();

        let sig_path2 = dir.path().join("sig2.json");
        let sig2 = sign_manifest(&manifest_path, &key_path, &sig_path2, false).unwrap();

        // Same seed must produce the same public key
        assert_eq!(sig1.public_key, sig2.public_key);

        // Same seed + same message must produce the same signature
        // (Ed25519 signatures are deterministic per RFC 8032)
        assert_eq!(sig1.signature, sig2.signature);

        // Same manifest hash
        assert_eq!(sig1.manifest_sha256, sig2.manifest_sha256);
    }

    #[test]
    fn different_seeds_produce_different_keys() {
        use ed25519_dalek::SigningKey;

        let seed_a = test_key_bytes();
        let mut seed_b = test_key_bytes();
        seed_b[0] ^= 0xFF; // Flip one byte

        let key_a = SigningKey::from_bytes(&seed_a);
        let key_b = SigningKey::from_bytes(&seed_b);

        assert_ne!(
            key_a.verifying_key().as_bytes(),
            key_b.verifying_key().as_bytes(),
            "different seeds should produce different public keys"
        );
    }

    // --- Nonce/IV uniqueness: Ed25519 is deterministic, but age uses random nonces ---

    #[test]
    fn age_encrypts_same_plaintext_differently_each_time() {
        if check_age_available().is_err() {
            eprintln!("Skipping: age CLI not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some((_identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
            eprintln!("Skipping: age-keygen not available");
            return;
        };

        let plaintext = b"identical plaintext for nonce test";

        // Encrypt twice
        let input1 = dir.path().join("input1.bin");
        std::fs::write(&input1, plaintext).unwrap();
        let enc1 = encrypt_with_age(&input1, std::slice::from_ref(&recipient)).unwrap();

        let input2 = dir.path().join("input2.bin");
        std::fs::write(&input2, plaintext).unwrap();
        let enc2 = encrypt_with_age(&input2, &[recipient]).unwrap();

        let ciphertext1 = std::fs::read(&enc1).unwrap();
        let ciphertext2 = std::fs::read(&enc2).unwrap();

        // Ciphertexts should differ due to random nonce/file key
        assert_ne!(
            ciphertext1, ciphertext2,
            "age should use unique nonces; encrypting the same plaintext twice must produce different ciphertexts"
        );
    }

    // --- Tampered authentication tag / SRI detection ---

    #[test]
    fn tampered_sri_hash_detected() {
        let dir = tempfile::tempdir().unwrap();
        let vendor_dir = dir.path().join("viewer").join("vendor");
        std::fs::create_dir_all(&vendor_dir).unwrap();
        std::fs::write(vendor_dir.join("app.js"), b"console.log('hello')").unwrap();

        // Write manifest with a deliberately wrong SRI hash
        let manifest = serde_json::json!({
            "viewer": {
                "sri": {
                    "vendor/app.js": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                }
            }
        });
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();

        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.sri_checked);
        assert!(!result.sri_valid, "tampered SRI hash should be detected");
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("SRI mismatch"),
            "error should mention SRI mismatch"
        );
    }

    #[test]
    fn tampered_file_content_detected_via_sri() {
        let dir = tempfile::tempdir().unwrap();
        let vendor_dir = dir.path().join("viewer").join("vendor");
        std::fs::create_dir_all(&vendor_dir).unwrap();

        let original_content = b"original script content";
        std::fs::write(vendor_dir.join("lib.js"), original_content).unwrap();

        // Compute correct SRI for original
        let correct_sri = format!("sha256-{}", base64_encode(&sha256_bytes(original_content)));
        let manifest = serde_json::json!({
            "viewer": {
                "sri": {
                    "vendor/lib.js": correct_sri
                }
            }
        });
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();

        // Verify passes with original content
        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.sri_checked);
        assert!(result.sri_valid);

        // Now tamper with the file
        std::fs::write(vendor_dir.join("lib.js"), b"tampered script content").unwrap();

        // Verify should fail
        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.sri_checked);
        assert!(
            !result.sri_valid,
            "tampered file content should fail SRI verification"
        );
    }

    // --- Age encrypt/decrypt roundtrip for different bundle types ---

    #[test]
    fn age_roundtrip_empty_file() {
        if check_age_available().is_err() {
            eprintln!("Skipping: age CLI not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
            eprintln!("Skipping: age-keygen not available");
            return;
        };

        let input = dir.path().join("empty.bin");
        std::fs::write(&input, b"").unwrap();

        let encrypted = encrypt_with_age(&input, &[recipient]).unwrap();
        let output = dir.path().join("empty.decrypted.bin");
        decrypt_with_age(&encrypted, &output, Some(&identity_path), None).unwrap();

        let decrypted = std::fs::read(&output).unwrap();
        assert!(decrypted.is_empty(), "decrypted empty file should be empty");
    }

    #[test]
    fn age_roundtrip_large_payload() {
        if check_age_available().is_err() {
            eprintln!("Skipping: age CLI not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
            eprintln!("Skipping: age-keygen not available");
            return;
        };

        // 1MB+ payload
        let large_data: Vec<u8> = (0..1_100_000u32).map(|i| (i % 256) as u8).collect();
        let input = dir.path().join("large.bin");
        std::fs::write(&input, &large_data).unwrap();

        let encrypted = encrypt_with_age(&input, &[recipient]).unwrap();

        // Encrypted file should be larger (header + auth tag overhead)
        let enc_size = std::fs::metadata(&encrypted).unwrap().len();
        assert!(
            enc_size > large_data.len() as u64,
            "encrypted file should be larger than plaintext"
        );

        let output = dir.path().join("large.decrypted.bin");
        decrypt_with_age(&encrypted, &output, Some(&identity_path), None).unwrap();

        let decrypted = std::fs::read(&output).unwrap();
        assert_eq!(
            decrypted, large_data,
            "large payload roundtrip should match"
        );
    }

    #[test]
    fn age_roundtrip_binary_data() {
        if check_age_available().is_err() {
            eprintln!("Skipping: age CLI not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
            eprintln!("Skipping: age-keygen not available");
            return;
        };

        // Binary data with all byte values 0x00..0xFF
        let binary_data: Vec<u8> = (0..=255).collect();
        let input = dir.path().join("binary.dat");
        std::fs::write(&input, &binary_data).unwrap();

        let encrypted = encrypt_with_age(&input, &[recipient]).unwrap();
        let output = dir.path().join("binary.decrypted.dat");
        decrypt_with_age(&encrypted, &output, Some(&identity_path), None).unwrap();

        let decrypted = std::fs::read(&output).unwrap();
        assert_eq!(decrypted, binary_data, "binary data roundtrip should match");
    }

    #[test]
    fn age_roundtrip_zip_bundle() {
        if check_age_available().is_err() {
            eprintln!("Skipping: age CLI not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
            eprintln!("Skipping: age-keygen not available");
            return;
        };

        // Simulate a ZIP bundle (just use the ZIP magic bytes + some data)
        let mut zip_data = vec![0x50, 0x4B, 0x03, 0x04]; // ZIP local file header magic
        zip_data.extend_from_slice(&[0u8; 1024]);
        let input = dir.path().join("bundle.zip");
        std::fs::write(&input, &zip_data).unwrap();

        let encrypted = encrypt_with_age(&input, &[recipient]).unwrap();
        assert!(
            encrypted.display().to_string().ends_with(".zip.age"),
            "encrypted zip should have .zip.age extension"
        );

        let output = dir.path().join("bundle.decrypted.zip");
        decrypt_with_age(&encrypted, &output, Some(&identity_path), None).unwrap();

        let decrypted = std::fs::read(&output).unwrap();
        assert_eq!(decrypted, zip_data, "ZIP bundle roundtrip should match");
    }

    #[test]
    fn age_roundtrip_full_bundle_zip_preserves_manifest_and_verification() {
        if check_age_available().is_err() {
            eprintln!("Skipping: age CLI not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
            eprintln!("Skipping: age-keygen not available");
            return;
        };

        let source = dir.path().join("full_bundle_source");
        std::fs::create_dir_all(&source).unwrap();
        let explicit_pubkey = write_signed_bundle_fixture(&source, "full");

        let zip_path = dir.path().join("full_bundle.zip");
        crate::package_directory_as_zip(&source, &zip_path).unwrap();
        let original_zip = std::fs::read(&zip_path).unwrap();

        let encrypted = encrypt_with_age(&zip_path, &[recipient]).unwrap();
        let decrypted_zip_path = dir.path().join("full_bundle.decrypted.zip");
        decrypt_with_age(&encrypted, &decrypted_zip_path, Some(&identity_path), None).unwrap();

        let decrypted_zip = std::fs::read(&decrypted_zip_path).unwrap();
        assert_eq!(
            decrypted_zip, original_zip,
            "decrypted full bundle zip should match original bytes"
        );

        let extracted = dir.path().join("full_bundle_extracted");
        std::fs::create_dir_all(&extracted).unwrap();
        extract_zip_archive(&decrypted_zip_path, &extracted);

        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(extracted.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(
            manifest.get("bundle_type").and_then(|v| v.as_str()),
            Some("full")
        );

        let verify = verify_bundle(&extracted, Some(&explicit_pubkey)).unwrap();
        assert!(verify.sri_checked);
        assert!(verify.sri_valid);
        assert!(verify.signature_checked);
        assert!(verify.signature_verified);
        assert_eq!(verify.key_source.as_deref(), Some("explicit"));
    }

    #[test]
    fn age_roundtrip_incremental_bundle_zip_preserves_manifest_and_verification() {
        if check_age_available().is_err() {
            eprintln!("Skipping: age CLI not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
            eprintln!("Skipping: age-keygen not available");
            return;
        };

        let source = dir.path().join("incremental_bundle_source");
        std::fs::create_dir_all(&source).unwrap();
        let explicit_pubkey = write_signed_bundle_fixture(&source, "incremental");

        let zip_path = dir.path().join("incremental_bundle.zip");
        crate::package_directory_as_zip(&source, &zip_path).unwrap();
        let original_zip = std::fs::read(&zip_path).unwrap();

        let encrypted = encrypt_with_age(&zip_path, &[recipient]).unwrap();
        let decrypted_zip_path = dir.path().join("incremental_bundle.decrypted.zip");
        decrypt_with_age(&encrypted, &decrypted_zip_path, Some(&identity_path), None).unwrap();

        let decrypted_zip = std::fs::read(&decrypted_zip_path).unwrap();
        assert_eq!(
            decrypted_zip, original_zip,
            "decrypted incremental bundle zip should match original bytes"
        );

        let extracted = dir.path().join("incremental_bundle_extracted");
        std::fs::create_dir_all(&extracted).unwrap();
        extract_zip_archive(&decrypted_zip_path, &extracted);

        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(extracted.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(
            manifest.get("bundle_type").and_then(|v| v.as_str()),
            Some("incremental")
        );

        let verify = verify_bundle(&extracted, Some(&explicit_pubkey)).unwrap();
        assert!(verify.sri_checked);
        assert!(verify.sri_valid);
        assert!(verify.signature_checked);
        assert!(verify.signature_verified);
        assert_eq!(verify.key_source.as_deref(), Some("explicit"));
    }

    // --- Wrong key decryption failure for age ---

    #[test]
    fn age_decrypt_with_wrong_identity_fails() {
        if check_age_available().is_err() {
            eprintln!("Skipping: age CLI not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();

        // Generate two separate identities
        let id_dir_a = dir.path().join("id_a");
        std::fs::create_dir_all(&id_dir_a).unwrap();
        let Some((_identity_a, recipient_a)) = try_generate_age_identity(&id_dir_a) else {
            eprintln!("Skipping: age-keygen not available");
            return;
        };

        let id_dir_b = dir.path().join("id_b");
        std::fs::create_dir_all(&id_dir_b).unwrap();
        let Some((identity_b, _recipient_b)) = try_generate_age_identity(&id_dir_b) else {
            eprintln!("Skipping: age-keygen not available");
            return;
        };

        // Encrypt with recipient A
        let input = dir.path().join("secret.txt");
        std::fs::write(&input, b"secret data").unwrap();
        let encrypted = encrypt_with_age(&input, &[recipient_a]).unwrap();

        // Try to decrypt with identity B (wrong key)
        let output = dir.path().join("decrypted.txt");
        let result = decrypt_with_age(&encrypted, &output, Some(&identity_b), None);
        assert!(
            result.is_err(),
            "decryption with wrong identity should fail"
        );
    }

    // --- Corrupted ciphertext detection for age ---

    #[test]
    fn age_corrupted_ciphertext_fails_decrypt() {
        if check_age_available().is_err() {
            eprintln!("Skipping: age CLI not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
            eprintln!("Skipping: age-keygen not available");
            return;
        };

        let input = dir.path().join("data.bin");
        std::fs::write(&input, b"important data").unwrap();
        let encrypted = encrypt_with_age(&input, &[recipient]).unwrap();

        // Corrupt the ciphertext by flipping bytes in the middle
        let mut ciphertext = std::fs::read(&encrypted).unwrap();
        if ciphertext.len() > 100 {
            // Corrupt bytes deep in the payload (past the header)
            for i in 80..std::cmp::min(100, ciphertext.len()) {
                ciphertext[i] ^= 0xFF;
            }
        }
        let corrupted_path = dir.path().join("corrupted.age");
        std::fs::write(&corrupted_path, &ciphertext).unwrap();

        let output = dir.path().join("decrypted.bin");
        let result = decrypt_with_age(&corrupted_path, &output, Some(&identity_path), None);
        assert!(
            result.is_err(),
            "decryption of corrupted ciphertext should fail"
        );
    }

    // --- Age encryption parameter validation ---

    #[test]
    fn age_encrypt_no_recipients_fails() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("test.bin");
        std::fs::write(&input, b"data").unwrap();

        let result = encrypt_with_age(&input, &[]);
        assert!(result.is_err(), "encryption with no recipients should fail");
    }

    #[test]
    fn age_decrypt_both_identity_and_passphrase_fails() {
        let dir = tempfile::tempdir().unwrap();
        let encrypted = dir.path().join("test.age");
        std::fs::write(&encrypted, b"fake").unwrap();
        let output = dir.path().join("out.bin");
        let id_path = dir.path().join("id.txt");
        std::fs::write(&id_path, b"fake identity").unwrap();

        let result = decrypt_with_age(&encrypted, &output, Some(&id_path), Some("password"));
        assert!(result.is_err(), "cannot combine identity and passphrase");
    }

    #[test]
    fn age_decrypt_neither_identity_nor_passphrase_fails() {
        let dir = tempfile::tempdir().unwrap();
        let encrypted = dir.path().join("test.age");
        std::fs::write(&encrypted, b"fake").unwrap();
        let output = dir.path().join("out.bin");

        let result = decrypt_with_age(&encrypted, &output, None, None);
        assert!(
            result.is_err(),
            "must provide either identity or passphrase"
        );
    }

    // --- SHA256 and base64 helpers: edge cases ---

    #[test]
    fn hex_sha256_empty_input() {
        let hash = hex_sha256(b"");
        // SHA-256 of empty string is well-known
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_bytes_matches_hex_sha256() {
        let data = b"cross-check";
        let hex_hash = hex_sha256(data);
        let raw_bytes = sha256_bytes(data);
        let hex_from_bytes = hex::encode(&raw_bytes);
        assert_eq!(hex_hash, hex_from_bytes);
    }

    #[test]
    fn base64_roundtrip_empty() {
        let encoded = base64_encode(b"");
        let decoded = base64_decode(&encoded).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn base64_roundtrip_all_byte_values() {
        let data: Vec<u8> = (0..=255).collect();
        let encoded = base64_encode(&data);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_decode_invalid_input() {
        let result = base64_decode("not valid base64!!!");
        assert!(result.is_err());
    }

    // --- Signature field integrity ---

    #[test]
    fn signature_metadata_fields_populated() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"check":"fields"}"#).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        assert_eq!(sig.algorithm, "ed25519");
        assert!(!sig.signature.is_empty());
        assert!(!sig.manifest_sha256.is_empty());
        assert!(!sig.public_key.is_empty());
        assert!(!sig.generated_at.is_empty());

        // Signature should be valid base64 decoding to 64 bytes
        let sig_bytes = base64_decode(&sig.signature).unwrap();
        assert_eq!(sig_bytes.len(), 64, "Ed25519 signature must be 64 bytes");

        // Public key should be valid base64 decoding to 32 bytes
        let pk_bytes = base64_decode(&sig.public_key).unwrap();
        assert_eq!(pk_bytes.len(), 32, "Ed25519 public key must be 32 bytes");

        // manifest_sha256 should be a 64-character hex string
        assert_eq!(sig.manifest_sha256.len(), 64);
        assert!(sig.manifest_sha256.chars().all(|c| c.is_ascii_hexdigit()));

        // generated_at should be a parseable timestamp
        assert!(
            chrono::DateTime::parse_from_rfc3339(&sig.generated_at).is_ok(),
            "generated_at should be valid RFC3339"
        );
    }

    #[test]
    fn sign_overwrite_produces_valid_signature() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        let key_path = dir.path().join("test.key");
        let sig_path = dir.path().join("manifest.sig.json");

        std::fs::write(&key_path, test_key_bytes()).unwrap();

        // Sign first version
        std::fs::write(&manifest_path, r#"{"version": 1}"#).unwrap();
        sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        // Update manifest and re-sign with overwrite
        std::fs::write(&manifest_path, r#"{"version": 2}"#).unwrap();
        let sig2 = sign_manifest(&manifest_path, &key_path, &sig_path, true).unwrap();

        // Verify the new signature matches the new manifest
        let result = verify_bundle(dir.path(), Some(&sig2.public_key)).unwrap();
        assert!(result.signature_checked);
        assert!(
            result.signature_verified,
            "overwritten signature should verify against updated manifest"
        );
    }

    // --- Combined sign + SRI verification ---

    #[test]
    fn full_bundle_sign_and_sri_verify() {
        let dir = tempfile::tempdir().unwrap();

        // Set up viewer files with SRI
        let vendor_dir = dir.path().join("viewer").join("vendor");
        std::fs::create_dir_all(&vendor_dir).unwrap();
        let js_content = b"function main() { return 42; }";
        let css_content = b"body { margin: 0; }";
        std::fs::write(vendor_dir.join("app.js"), js_content).unwrap();
        std::fs::write(vendor_dir.join("style.css"), css_content).unwrap();

        let js_sri = format!("sha256-{}", base64_encode(&sha256_bytes(js_content)));
        let css_sri = format!("sha256-{}", base64_encode(&sha256_bytes(css_content)));

        let manifest = serde_json::json!({
            "schema_version": "0.1.0",
            "viewer": {
                "sri": {
                    "vendor/app.js": js_sri,
                    "vendor/style.css": css_sri,
                }
            }
        });
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        // Sign the manifest
        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();
        let sig_path = dir.path().join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        // Full verify: SRI + signature
        let result = verify_bundle(dir.path(), Some(&sig.public_key)).unwrap();
        assert!(result.sri_checked);
        assert!(result.sri_valid, "SRI should be valid");
        assert!(result.signature_checked);
        assert!(result.signature_verified, "signature should verify");
        assert!(result.error.is_none());
        assert_eq!(result.key_source.as_deref(), Some("explicit"));
    }

    #[test]
    fn tampered_public_key_in_sig_file_fails_with_explicit_key() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"test":"tampered pk"}"#).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let sig_path = dir.path().join("manifest.sig.json");
        let sig = sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();

        // Create a different key pair and use its public key as "explicit"
        let other_seed: [u8; 32] = [0xAA; 32];
        let other_key = ed25519_dalek::SigningKey::from_bytes(&other_seed);
        let other_pub = base64_encode(other_key.verifying_key().as_bytes());

        // The signature was made with original key, but we verify with other key
        let result = verify_bundle(dir.path(), Some(&other_pub)).unwrap();
        assert!(result.signature_checked);
        assert!(
            !result.signature_verified,
            "verification with mismatched explicit key should fail"
        );

        // But embedded key should still work (self-consistency)
        let result2 = verify_bundle(dir.path(), None).unwrap();
        assert!(result2.signature_checked);
        assert!(
            result2.signature_verified,
            "embedded key should still verify (self-signed trust model)"
        );
        assert_eq!(result2.key_source.as_deref(), Some("embedded"));

        // And the original correct explicit key should work
        let result3 = verify_bundle(dir.path(), Some(&sig.public_key)).unwrap();
        assert!(result3.signature_checked);
        assert!(result3.signature_verified);
    }
}
