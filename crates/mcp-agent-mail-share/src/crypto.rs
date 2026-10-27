//! Cryptographic operations for bundle signing and encryption.
//!
//! - Ed25519 manifest signing via `ed25519-dalek`
//! - Age recipient encryption and identity-file decryption via the CLI
//! - In-process age passphrase decryption with authenticated private staging

#[cfg(test)]
use std::cell::RefCell;
use std::ffi::OsStr;
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ShareError, ShareResult};

#[cfg(test)]
thread_local! {
    static AGE_COMMAND_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static CRYPTO_PUBLICATION_HOOK: RefCell<Option<CryptoPublicationHook>> = const { RefCell::new(None) };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CryptoPublicationPhase {
    Captured,
    BeforePublish,
}

#[cfg(test)]
type CryptoPublicationHook = Box<dyn FnMut(CryptoPublicationPhase, &Path, &Path)>;

#[cfg(test)]
fn with_crypto_publication_hook<R>(
    hook: impl FnMut(CryptoPublicationPhase, &Path, &Path) + 'static,
    operation: impl FnOnce() -> R,
) -> R {
    struct Restore(Option<CryptoPublicationHook>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CRYPTO_PUBLICATION_HOOK.with(|slot| {
                *slot.borrow_mut() = self.0.take();
            });
        }
    }
    let previous = CRYPTO_PUBLICATION_HOOK.with(|slot| slot.replace(Some(Box::new(hook))));
    let _restore = Restore(previous);
    operation()
}

#[cfg(test)]
fn crypto_publication_hook(phase: CryptoPublicationPhase, output: &Path, stage: &Path) {
    CRYPTO_PUBLICATION_HOOK.with(|slot| {
        let hook = slot.borrow_mut().take();
        if let Some(mut hook) = hook {
            hook(phase, output, stage);
            *slot.borrow_mut() = Some(hook);
        }
    });
}

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
/// Publication retains private staged bytes on refusal. An authorized overwrite
/// replaces only the captured target; displaced files are retained for recovery.
pub fn sign_manifest(
    manifest_path: &Path,
    signing_key_path: &Path,
    output_path: &Path,
    overwrite: bool,
) -> ShareResult<ManifestSignature> {
    use ed25519_dalek::{Signer, SigningKey};

    if !crate::is_real_file(manifest_path) {
        return Err(ShareError::ManifestNotFound {
            path: manifest_path.display().to_string(),
        });
    }
    require_real_crypto_file(signing_key_path, "signing key")?;
    let mut manifest_file = mcp_agent_mail_core::disk::open_regular_file_no_follow(manifest_path)?;
    let key_file = mcp_agent_mail_core::disk::open_regular_file_no_follow(signing_key_path)?;
    let mut publication = CryptoOutput::capture(output_path, ".age-sign.", overwrite)?;
    let result = (|| {
        // Read signing key (32-byte seed or 64-byte expanded — use first 32)
        let mut key_bytes = Vec::new();
        key_file.take(65).read_to_end(&mut key_bytes)?;
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
            .unwrap_or_else(|_| unreachable!());
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();

        // Read and hash manifest
        let mut manifest_bytes = Vec::new();
        manifest_file.read_to_end(&mut manifest_bytes)?;
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
        let json =
            serde_json::to_string_pretty(&sig_meta).map_err(|e| ShareError::ManifestParse {
                message: e.to_string(),
            })?;
        publication.file_mut()?.write_all(json.as_bytes())?;
        publication.publish()?;

        Ok(sig_meta)
    })();
    result.map_err(|error| publication.retained_error(error))
}

/// Verify SRI hashes and Ed25519 signature for a bundle.
///
/// Returns verification results.
pub fn verify_bundle(
    bundle_root: &Path,
    public_key_b64: Option<&str>,
) -> ShareResult<VerifyResult> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    if !crate::is_real_dir(bundle_root) {
        return Err(ShareError::ManifestNotFound {
            path: bundle_root.display().to_string(),
        });
    }

    let manifest_path = bundle_root.join("manifest.json");
    if !crate::is_real_file(&manifest_path) {
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
    let mut sri_files_verified: usize = 0;
    if let Some(viewer) = manifest.get("viewer")
        && let Some(sri_map) = viewer.get("sri").and_then(|v| v.as_object())
    {
        sri_checked = true;
        for (relative_path, expected_sri) in sri_map {
            let Some(expected) = expected_sri.as_str() else {
                // Non-string SRI values (null, number, etc.) are invalid —
                // treat as verification failure to prevent bypass.
                return Ok(VerifyResult {
                    bundle: bundle_root.display().to_string(),
                    sri_checked: true,
                    sri_valid: false,
                    signature_checked: false,
                    signature_verified: false,
                    key_source: None,
                    database_checked: false,
                    database_verified: false,
                    files_checked: false,
                    files_verified: false,
                    error: Some(format!(
                        "SRI entry for {relative_path} has non-string value"
                    )),
                });
            };
            let file_path = match resolve_sri_file_path(bundle_root, relative_path) {
                Ok(p) => p,
                Err(traversal_err) => {
                    return Ok(VerifyResult {
                        bundle: bundle_root.display().to_string(),
                        sri_checked: true,
                        sri_valid: false,
                        signature_checked: false,
                        signature_verified: false,
                        key_source: None,
                        database_checked: false,
                        database_verified: false,
                        files_checked: false,
                        files_verified: false,
                        error: Some(traversal_err),
                    });
                }
            };
            if file_path.exists() {
                let content = std::fs::read(&file_path)?;
                let actual_hash = format!("sha256-{}", base64_encode(&sha256_bytes(&content)));
                // Use constant-length comparison to avoid timing oracles.
                // While SHA-256 makes this practically unexploitable, defense-in-depth
                // is cheap and prevents future regret.
                let hashes_match = actual_hash.len() == expected.len()
                    && actual_hash
                        .bytes()
                        .zip(expected.bytes())
                        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                        == 0;
                if !hashes_match {
                    return Ok(VerifyResult {
                        bundle: bundle_root.display().to_string(),
                        sri_checked: true,
                        sri_valid: false,
                        signature_checked: false,
                        signature_verified: false,
                        key_source: None,
                        database_checked: false,
                        database_verified: false,
                        files_checked: false,
                        files_verified: false,
                        error: Some(format!(
                            "SRI mismatch for {relative_path}: file content does not match manifest hash"
                        )),
                    });
                }
                sri_files_verified += 1;
            } else {
                return Ok(VerifyResult {
                    bundle: bundle_root.display().to_string(),
                    sri_checked: true,
                    sri_valid: false,
                    signature_checked: false,
                    signature_verified: false,
                    key_source: None,
                    database_checked: false,
                    database_verified: false,
                    files_checked: false,
                    files_verified: false,
                    error: Some(format!("SRI-referenced file missing: {relative_path}")),
                });
            }
        }
    }

    // Check Ed25519 signature (requires sig file to exist)
    let sig_path = bundle_root.join("manifest.sig.json");
    let mut signature_checked = false;
    let mut signature_verified = false;
    let mut key_source: Option<String> = None;

    match std::fs::symlink_metadata(&sig_path) {
        Ok(metadata) => {
            signature_checked = true;

            if metadata.file_type().is_symlink() {
                return Ok(VerifyResult {
                    bundle: bundle_root.display().to_string(),
                    sri_checked,
                    sri_valid: sri_checked && sri_files_verified > 0,
                    signature_checked: true,
                    signature_verified: false,
                    key_source: None,
                    database_checked: false,
                    database_verified: false,
                    files_checked: false,
                    files_verified: false,
                    error: Some("signature file must not be a symlink".to_string()),
                });
            }
            if !metadata.file_type().is_file() {
                return Ok(VerifyResult {
                    bundle: bundle_root.display().to_string(),
                    sri_checked,
                    sri_valid: sri_checked && sri_files_verified > 0,
                    signature_checked: true,
                    signature_verified: false,
                    key_source: None,
                    database_checked: false,
                    database_verified: false,
                    files_checked: false,
                    files_verified: false,
                    error: Some("signature file must be a regular file".to_string()),
                });
            }

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
                    .map(std::string::ToString::to_string);
                let source = embedded.as_ref().map(|_| "embedded".to_string());
                (embedded, source)
            };
            key_source = ks;

            let sig_str = sig_json.get("signature").and_then(|v| v.as_str());

            if let (Some(pk_b64), Some(sig_b64)) = (pub_key_str, sig_str)
                && let (Ok(pk_bytes), Ok(sig_bytes)) =
                    (base64_decode(&pk_b64), base64_decode(sig_b64))
                && pk_bytes.len() == 32
                && sig_bytes.len() == 64
            {
                let pk: [u8; 32] = pk_bytes.try_into().unwrap_or_else(|_| unreachable!());
                let sig: [u8; 64] = sig_bytes.try_into().unwrap_or_else(|_| unreachable!());
                if let Ok(verifying_key) = VerifyingKey::from_bytes(&pk) {
                    let signature = Signature::from_bytes(&sig);
                    signature_verified = verifying_key.verify(&manifest_bytes, &signature).is_ok();
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ShareError::Io(error)),
    }

    // Verify the database payload against the manifest's signed `database.sha256`.
    // The signature above only proves `manifest.json` is authentic; without this
    // check an attacker can swap `mailbox.sqlite3` (or its chunks) for arbitrary
    // content and the bundle would still verify "green".
    let (database_checked, database_verified, database_error) =
        match verify_database_payload(bundle_root, &manifest)? {
            Some(Ok(())) => (true, true, None),
            Some(Err(message)) => (true, false, Some(message)),
            None => (false, false, None),
        };
    // The same for every other file the export wrote: viewer code, rendered
    // pages and attachments are served to readers, so a signature that left
    // them out would bless a bundle whose viewer had been swapped.
    let (files_checked, files_verified, files_error) =
        match verify_listed_files(bundle_root, &manifest)? {
            Some(Ok(())) => (true, true, None),
            Some(Err(message)) => (true, false, Some(message)),
            None => (false, false, None),
        };

    Ok(VerifyResult {
        bundle: bundle_root.display().to_string(),
        sri_checked,
        sri_valid: sri_checked && sri_files_verified > 0,
        signature_checked,
        signature_verified,
        key_source,
        database_checked,
        database_verified,
        files_checked,
        files_verified,
        error: database_error.or(files_error),
    })
}

/// Check every file in the manifest's `files` map against its recorded
/// SHA-256. `Ok(None)` when the manifest has no map (exported before it
/// existed); otherwise `Ok(Some(Err(_)))` names the first listed file that is
/// missing, not a regular file inside the bundle, or altered. Files the map
/// does not list (hosting config, deploy history, an operator's `CNAME`) are
/// not judged.
fn verify_listed_files(
    bundle_root: &Path,
    manifest: &serde_json::Value,
) -> ShareResult<Option<Result<(), String>>> {
    let Some(files) = manifest.get(crate::bundle::MANIFEST_FILES_KEY) else {
        return Ok(None);
    };
    let Some(files) = files.as_object() else {
        return Ok(Some(Err(
            "manifest `files` must map bundle paths to SHA-256 hashes".to_string(),
        )));
    };
    for (relative_path, expected) in files {
        let Some(expected) = expected.as_str() else {
            return Ok(Some(Err(format!(
                "manifest hash for {relative_path} is not a string"
            ))));
        };
        if !Path::new(relative_path)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            return Ok(Some(Err(format!(
                "path traversal blocked: '{relative_path}' is not a plain bundle-relative path"
            ))));
        }
        let path = match validate_sri_resolved_path(
            bundle_root,
            bundle_root.join(relative_path),
            relative_path,
        ) {
            Ok(path) => path,
            Err(err) => return Ok(Some(Err(err))),
        };
        if !crate::is_real_file(&path) {
            return Ok(Some(Err(format!(
                "bundle file listed in the manifest is missing: {relative_path}"
            ))));
        }
        let mut hasher = Sha256::new();
        if let Err(err) = hash_file_into(&path, &mut hasher) {
            return Ok(Some(Err(format!("failed to read {relative_path}: {err}"))));
        }
        if !hex::encode(hasher.finalize()).eq_ignore_ascii_case(expected) {
            return Ok(Some(Err(format!(
                "{relative_path} does not match its hash in the manifest"
            ))));
        }
    }
    Ok(Some(Ok(())))
}

/// Verify the bundle's database payload against the manifest's signed
/// `database.sha256`.
///
/// Returns `Ok(None)` when there is nothing to check (no `database` section,
/// no recorded hash, or no database artifact present in the bundle),
/// `Ok(Some(Ok(())))` when the payload hash matches the manifest, and
/// `Ok(Some(Err(message)))` when it diverges or a referenced artifact is
/// missing/unreadable. For chunked databases the chunks are streamed in index
/// order — equivalent to hashing the reassembled file — so the comparison is
/// against the same `database.sha256` the exporter recorded.
fn verify_database_payload(
    bundle_root: &Path,
    manifest: &serde_json::Value,
) -> ShareResult<Option<Result<(), String>>> {
    let Some(database) = manifest.get("database") else {
        return Ok(None);
    };
    let Some(expected) = database.get("sha256").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let chunked = database
        .get("chunked")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let db_rel = database
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("mailbox.sqlite3");

    if chunked {
        let Some(chunk_count) = database
            .get("chunk_manifest")
            .and_then(|c| c.get("chunk_count"))
            .and_then(serde_json::Value::as_u64)
        else {
            return Ok(Some(Err(
                "chunked database manifest is missing chunk_count".to_string(),
            )));
        };
        // Verify the reassembled chunks against the signed hash: streamed in
        // index order, which is byte-identical to the original database file.
        let mut hasher = Sha256::new();
        for index in 0..chunk_count {
            let chunk_rel = format!("chunks/{index:05}.bin");
            let chunk_path = match resolve_sri_file_path(bundle_root, &chunk_rel) {
                Ok(path) => path,
                Err(err) => return Ok(Some(Err(err))),
            };
            if !crate::is_real_file(&chunk_path) {
                return Ok(Some(Err(format!("database chunk missing: {chunk_rel}"))));
            }
            if let Err(err) = hash_file_into(&chunk_path, &mut hasher) {
                return Ok(Some(Err(format!("failed to read {chunk_rel}: {err}"))));
            }
        }
        if !hex::encode(hasher.finalize()).eq_ignore_ascii_case(expected) {
            return Ok(Some(Err(
                "reassembled database chunks do not match the signed manifest hash".to_string(),
            )));
        }
        // A chunked bundle also ships the standalone `mailbox.sqlite3`; if present
        // it must match the same signed hash, so a swapped standalone DB (with the
        // chunks left intact) cannot pass verification either.
        return Ok(Some(
            verify_db_file_against_hash(bundle_root, db_rel, expected)?.unwrap_or(Ok(())),
        ));
    }

    // Non-chunked: verify the standalone database file. Absent (e.g. an encrypted
    // or metadata-only bundle) → not checked.
    verify_db_file_against_hash(bundle_root, db_rel, expected)
}

/// Hash the database file at `db_rel` (if present) and compare to `expected`.
///
/// Returns `Ok(None)` when the file is absent (nothing to verify),
/// `Ok(Some(Ok(())))` on a match, and `Ok(Some(Err(_)))` on a mismatch, an
/// unreadable file, or a rejected path (symlink / traversal).
fn verify_db_file_against_hash(
    bundle_root: &Path,
    db_rel: &str,
    expected: &str,
) -> ShareResult<Option<Result<(), String>>> {
    let db_path = match resolve_sri_file_path(bundle_root, db_rel) {
        Ok(path) => path,
        Err(err) => return Ok(Some(Err(err))),
    };
    if !crate::is_real_file(&db_path) {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    if let Err(err) = hash_file_into(&db_path, &mut hasher) {
        return Ok(Some(Err(format!("failed to read {db_rel}: {err}"))));
    }
    if hex::encode(hasher.finalize()).eq_ignore_ascii_case(expected) {
        Ok(Some(Ok(())))
    } else {
        Ok(Some(Err(
            "database payload does not match the signed manifest hash".to_string(),
        )))
    }
}

/// Stream a file into an existing SHA-256 hasher (bounded memory).
fn hash_file_into(path: &Path, hasher: &mut Sha256) -> std::io::Result<()> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(())
}

fn resolve_sri_file_path(bundle_root: &Path, relative_path: &str) -> Result<PathBuf, String> {
    // Reject obvious path traversal attempts.
    if relative_path.contains("..")
        || std::path::Path::new(relative_path).is_absolute()
        || relative_path.contains('\0')
    {
        return Err(format!(
            "path traversal blocked: '{relative_path}' contains prohibited components"
        ));
    }

    // Historical manifests store SRI paths relative to `viewer/` (e.g. `vendor/foo.js`),
    // while some tooling may emit bundle-root relative paths. Accept either.
    let direct = bundle_root.join(relative_path);
    if std::fs::symlink_metadata(&direct).is_ok() {
        return validate_sri_resolved_path(bundle_root, direct, relative_path);
    }
    validate_sri_resolved_path(
        bundle_root,
        bundle_root.join("viewer").join(relative_path),
        relative_path,
    )
}

fn validate_sri_resolved_path(
    bundle_root: &Path,
    candidate: PathBuf,
    relative_path: &str,
) -> Result<PathBuf, String> {
    let metadata = match std::fs::symlink_metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
        Err(error) => {
            return Err(format!("failed to inspect '{relative_path}': {error}"));
        }
    };

    if metadata.file_type().is_symlink() {
        return Err(format!(
            "bundle path must not be a symlink: '{relative_path}'"
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(format!(
            "bundle path must be a regular file: '{relative_path}'"
        ));
    }

    let canonical_root = bundle_root
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize bundle root: {error}"))?;
    let canonical_candidate = candidate
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize '{relative_path}': {error}"))?;
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(format!(
            "path traversal blocked: '{relative_path}' resolves outside bundle root"
        ));
    }
    Ok(candidate)
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
    /// Whether the database payload (`mailbox.sqlite3` or its chunks) was checked
    /// against the manifest's signed `database.sha256`.
    pub database_checked: bool,
    /// Whether the database payload matched the manifest's signed `database.sha256`.
    /// Only meaningful as authenticity when `signature_verified` is also true.
    pub database_verified: bool,
    /// Whether the manifest's per-file hash map (`files`) was checked. Bundles
    /// exported before the map existed carry none, and this stays false.
    #[serde(default)]
    pub files_checked: bool,
    /// Whether every file in that map matched its hash. As with the database,
    /// authenticity also needs `signature_verified`.
    #[serde(default)]
    pub files_verified: bool,
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

    require_real_crypto_file(input, "encryption input")?;
    let input_file = mcp_agent_mail_core::disk::open_regular_file_no_follow(input)?;

    let output = input.with_extension(input.extension().map_or_else(
        || "age".to_string(),
        |e| format!("{}.age", e.to_string_lossy()),
    ));

    let mut publication = CryptoOutput::capture(&output, ".age-encrypt.", false)?;
    let result = (|| {
        check_age_available()?;
        let mut cmd = age_command();
        for recipient in recipients {
            cmd.arg("-r").arg(recipient);
        }
        // The child inherits already-open objects. It never reopens an input
        // or output pathname after its parent authority has been captured.
        let status = cmd
            .stdin(std::process::Stdio::from(input_file))
            .stdout(std::process::Stdio::from(
                publication.file_mut()?.try_clone()?,
            ))
            .stderr(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            return Err(ShareError::Io(std::io::Error::other(
                "age encryption failed (check recipient and input)",
            )));
        }
        publication.publish()
    })();
    result.map_err(|error| publication.retained_error(error))?;
    Ok(output)
}

/// Decrypt an age-encrypted file.
///
/// Provide either `identity` (path to age identity file) or `passphrase`.
/// Passphrases stay in-process; identity files retain the `age` CLI's support
/// for identity formats and plugins. Plaintext is published only after the
/// entire authenticated stream has been read successfully.
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

    require_real_crypto_file(encrypted_path, "encrypted input")?;
    if let Some(id_path) = identity {
        require_real_crypto_file(id_path, "identity file")?;
    }
    let input_file = mcp_agent_mail_core::disk::open_regular_file_no_follow(encrypted_path)?;
    let mut publication = CryptoOutput::capture(output_path, ".age-decrypt.", false)?;
    let result = (|| {
        if let Some(id_path) = identity {
            check_age_available()?;
            let status = age_command()
                .arg("-d")
                .arg("-i")
                .arg(id_path)
                .stdin(std::process::Stdio::from(input_file))
                .stdout(std::process::Stdio::from(
                    publication.file_mut()?.try_clone()?,
                ))
                .stderr(std::process::Stdio::null())
                .status()?;
            if !status.success() {
                // Malformed identities and plugins may include secret material in
                // diagnostics. Keep the CLI's identity support without relaying it.
                return Err(ShareError::Io(std::io::Error::other(
                    "age decryption failed (check identity and file integrity)",
                )));
            }
        } else if let Some(pass) = passphrase {
            decrypt_age_passphrase_to_staging(input_file, publication.file_mut()?, pass)?;
        } else {
            // Unreachable because we validated inputs above, but keep a defensive branch.
            return Err(ShareError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "either identity or passphrase required for decryption",
            )));
        }

        publication.publish()
    })();
    result.map_err(|error| publication.retained_error(error))
}

fn decrypt_age_passphrase_to_staging(
    input: std::fs::File,
    output: &mut std::fs::File,
    passphrase: &str,
) -> ShareResult<()> {
    // ArmoredReader accepts both binary age files and ASCII-armored files,
    // matching the external CLI. No secret ever crosses a subprocess boundary.
    let decryptor = age::Decryptor::new(age::armor::ArmoredReader::new(input))
        .map_err(|_| age_passphrase_decryption_error())?;
    if !decryptor.is_scrypt() {
        return Err(age_passphrase_decryption_error());
    }

    let mut identity =
        age::scrypt::Identity::new(age::secrecy::SecretString::from(passphrase.to_owned()));
    // Match official age's maximum, bounding an untrusted header's CPU/memory
    // request independently of the library's machine-speed calibration.
    identity.set_max_work_factor(22);
    let mut plaintext = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|_| age_passphrase_decryption_error())?;
    drop(identity);

    // Authenticating the header is insufficient: a late corrupt/truncated
    // chunk must fail before the caller publishes any plaintext. The private
    // retained staging directory contains any partial bytes for recovery.
    std::io::copy(&mut plaintext, output).map_err(|_| age_passphrase_decryption_error())?;
    Ok(())
}

fn age_passphrase_decryption_error() -> ShareError {
    // Do not expose dependency diagnostics or any secret-bearing input.
    ShareError::Io(std::io::Error::other(
        "age decryption failed (check passphrase and file integrity)",
    ))
}

fn crypto_refusal(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

/// A captured regular file. Hashing is streamed and stops at the captured
/// length plus one byte, even if another writer keeps extending the file.
struct CryptoFileWitness {
    #[cfg(not(windows))]
    object: same_file::Handle,
    #[cfg(windows)]
    object: (u64, u64),
    #[cfg(windows)]
    retained: Option<std::fs::File>,
    state: CryptoFileState,
    sha256: [u8; 32],
}

#[derive(PartialEq, Eq)]
struct CryptoFileState {
    len: u64,
    links: u64,
    mode: u32,
    modified: std::time::SystemTime,
}

fn crypto_file_state(file: &std::fs::File) -> std::io::Result<CryptoFileState> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(crypto_refusal("crypto artifact must be a regular file"));
    }
    #[cfg(unix)]
    let (links, mode) = {
        use std::os::unix::fs::MetadataExt as _;
        (metadata.nlink(), metadata.mode())
    };
    #[cfg(windows)]
    let (links, mode) = {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return Err(crypto_refusal(
                "crypto artifact must not be a reparse point",
            ));
        }
        (
            mcp_agent_mail_core::disk::windows_file_link_count(file)?,
            u32::from(metadata.permissions().readonly()),
        )
    };
    #[cfg(not(any(unix, windows)))]
    let (links, mode) = (1, u32::from(metadata.permissions().readonly()));
    Ok(CryptoFileState {
        len: metadata.len(),
        links,
        mode,
        modified: metadata.modified()?,
    })
}

fn crypto_file_witness(file: &std::fs::File) -> std::io::Result<CryptoFileWitness> {
    let state = crypto_file_state(file)?;
    #[cfg(not(windows))]
    let object = same_file::Handle::from_file(file.try_clone()?)?;
    #[cfg(windows)]
    let object = {
        let metadata = cap_std::fs::File::from_std(file.try_clone()?).metadata()?;
        (
            cap_fs_ext::MetadataExt::dev(&metadata),
            cap_fs_ext::MetadataExt::ino(&metadata),
        )
    };
    let mut reader = file.try_clone()?;
    reader.rewind()?;
    let mut reader = reader.take(state.len.saturating_add(1));
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut count = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count = count.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        digest.update(&buffer[..read]);
    }
    if count != state.len || crypto_file_state(file)? != state {
        return Err(crypto_refusal(
            "crypto artifact changed while being inspected",
        ));
    }
    Ok(CryptoFileWitness {
        object,
        #[cfg(windows)]
        retained: Some(file.try_clone()?),
        state,
        sha256: digest.finalize().into(),
    })
}

impl CryptoFileWitness {
    fn matches(&self, other: &Self, additional_links: u64) -> bool {
        self.object == other.object
            && self.sha256 == other.sha256
            && self.state.len == other.state.len
            && self.state.mode == other.state.mode
            && self.state.modified == other.state.modified
            && self.state.links.checked_add(additional_links) == Some(other.state.links)
    }
}

fn open_crypto_leaf(directory: &cap_std::fs::Dir, name: &OsStr) -> std::io::Result<std::fs::File> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed());
    }
    let file = directory.open_with(name, &options)?.into_std();
    crypto_file_state(&file)?;
    Ok(file)
}

fn crypto_named_witness(
    directory: &cap_std::fs::Dir,
    name: &OsStr,
) -> std::io::Result<CryptoFileWitness> {
    crypto_file_witness(&open_crypto_leaf(directory, name)?)
}

#[cfg(unix)]
fn sync_crypto_directory(directory: &cap_std::fs::Dir) -> std::io::Result<()> {
    // cap-std directory authorities use O_PATH on Linux, which cannot be
    // fsynced. Open a readable descriptor relative to that same authority;
    // reopening its ambient pathname could synchronize a substituted parent.
    directory.open(".")?.sync_all()
}

/// Open each component relative to its retained predecessor. Windows directory
/// handles omit FILE_SHARE_DELETE; retaining every ancestor also pins their names.
fn open_crypto_directory(
    path: &Path,
    create: bool,
) -> std::io::Result<(cap_std::fs::Dir, Vec<cap_std::fs::Dir>)> {
    use cap_fs_ext::DirExt as _;
    use std::path::Component;
    let mut anchor = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => anchor.push(prefix.as_os_str()),
            Component::RootDir => anchor.push(component.as_os_str()),
            _ => break,
        }
    }
    if !path.is_absolute() || anchor.as_os_str().is_empty() {
        return Err(crypto_refusal("crypto output parent must be absolute"));
    }
    let mut current = cap_std::fs::Dir::open_ambient_dir(&anchor, cap_std::ambient_authority())?;
    let mut ancestors = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                return Err(crypto_refusal(
                    "crypto output path must not traverse a parent",
                ));
            }
            Component::Normal(name) => {
                match current.symlink_metadata(name) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(crypto_refusal(
                            "crypto output directory must not be a symlink",
                        ));
                    }
                    Ok(metadata) if !metadata.file_type().is_dir() => {
                        return Err(crypto_refusal("crypto output parent must be a directory"));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                let next = match current.open_dir_nofollow(name) {
                    Ok(next) => next,
                    Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                        match current.create_dir(name) {
                            Ok(()) => {
                                // Persist the new directory's name before
                                // descending: syncing the child alone cannot
                                // make this entry durable in its parent.
                                #[cfg(unix)]
                                sync_crypto_directory(&current)?;
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                            Err(error) => return Err(error),
                        }
                        current.open_dir_nofollow(name)?
                    }
                    Err(error) => return Err(error),
                };
                ancestors.push(current);
                current = next;
            }
        }
    }
    Ok((current, ancestors))
}

fn absolute_crypto_output(path: &Path) -> std::io::Result<PathBuf> {
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(crypto_refusal(
            "crypto output path must not traverse a parent",
        ));
    }
    let absolute = std::path::absolute(path)?;
    #[cfg(target_os = "macos")]
    for (alias, target) in [
        ("/tmp", "/private/tmp"),
        ("/var", "/private/var"),
        ("/etc", "/private/etc"),
    ] {
        let alias = Path::new(alias);
        if absolute.starts_with(alias)
            && mcp_agent_mail_core::disk::is_trusted_system_directory_alias(alias)
        {
            return Ok(Path::new(target).join(
                absolute
                    .strip_prefix(alias)
                    .map_err(|_| crypto_refusal("invalid system-directory alias"))?,
            ));
        }
    }
    Ok(absolute)
}

/// All mutation stays under these retained directory/file handles. No destructor
/// removes artifacts by pathname. A late parent substitution can leave a complete
/// output in the original directory; it returns publication-unconfirmed and never
/// touches the substitute directory or retries there.
struct CryptoOutput {
    output: PathBuf,
    parent_path: PathBuf,
    name: std::ffi::OsString,
    parent: cap_std::fs::Dir,
    _ancestors: Vec<cap_std::fs::Dir>,
    stage: cap_std::fs::Dir,
    stage_path: PathBuf,
    file: Option<std::fs::File>,
    expected: Option<CryptoFileWitness>,
}

impl CryptoOutput {
    fn capture(output: &Path, prefix: &str, overwrite: bool) -> ShareResult<Self> {
        use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
        let output = absolute_crypto_output(output)?;
        let name = output
            .file_name()
            .ok_or_else(|| crypto_refusal("crypto output must name a file"))?
            .to_os_string();
        #[cfg(windows)]
        mcp_agent_mail_core::journal_io::validate_entry_name(name.to_str().ok_or_else(|| {
            crypto_refusal("Windows crypto output requires a Unicode file name")
        })?)?;
        let parent_path = output
            .parent()
            .ok_or_else(|| crypto_refusal("crypto output has no parent"))?
            .to_path_buf();
        let (parent, ancestors) = open_crypto_directory(&parent_path, true)?;
        let expected = match parent.symlink_metadata(&name) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(crypto_refusal("crypto output must not be a symlink").into());
                }
                if !metadata.file_type().is_file() {
                    return Err(crypto_refusal("crypto output must be a regular file").into());
                }
                if !overwrite {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "crypto output already exists",
                    )
                    .into());
                }
                Some(crypto_named_witness(&parent, &name)?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let temp = tempfile::Builder::new()
            .prefix(prefix)
            .rand_bytes(16)
            .disable_cleanup(true)
            .make_in(&parent_path, |candidate| {
                let name = candidate
                    .file_name()
                    .ok_or_else(|| crypto_refusal("invalid staging name"))?;
                let builder = cap_std::fs::DirBuilder::new();
                #[cfg(unix)]
                let builder = {
                    use cap_std::fs::DirBuilderExt as _;
                    let mut builder = builder;
                    builder.mode(0o700);
                    builder
                };
                parent.create_dir_with(name, &builder)?;
                let directory = parent.open_dir_nofollow(name)?;
                let mut options = cap_std::fs::OpenOptions::new();
                options
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .follow(FollowSymlinks::No);
                #[cfg(unix)]
                {
                    use cap_std::fs::OpenOptionsExt as _;
                    options.mode(0o600);
                }
                let file = directory.open_with("payload", &options)?.into_std();
                Ok((directory, file))
            })?;
        let stage_path = temp.path().to_path_buf();
        let ((stage, file), _retained_path) = temp.into_parts();
        let publication = Self {
            output,
            parent_path,
            name,
            parent,
            _ancestors: ancestors,
            stage,
            stage_path,
            file: Some(file),
            expected,
        };
        #[cfg(test)]
        crypto_publication_hook(
            CryptoPublicationPhase::Captured,
            &publication.output,
            &publication.stage_path.join("payload"),
        );
        Ok(publication)
    }

    fn validate_parent(&self) -> std::io::Result<()> {
        let (observed, _ancestors) = open_crypto_directory(&self.parent_path, false)?;
        let captured = same_file::Handle::from_file(self.parent.try_clone()?.into_std_file())?;
        let observed = same_file::Handle::from_file(observed.into_std_file())?;
        if captured != observed {
            return Err(crypto_refusal("crypto output parent changed identity"));
        }
        Ok(())
    }

    fn retained_error(&self, error: ShareError) -> ShareError {
        let kind = match &error {
            ShareError::Io(error) => error.kind(),
            _ => std::io::ErrorKind::Other,
        };
        std::io::Error::new(kind, format!("{error}; crypto artifacts for {} retained under {} (under the original parent if it moved)", self.output.display(), self.stage_path.display())).into()
    }

    fn unconfirmed(&self) -> std::io::Error {
        std::io::Error::other(
            "crypto publication unconfirmed; original directory and all retained artifacts must be inspected before retrying",
        )
    }

    fn sync_directories(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            sync_crypto_directory(&self.stage)?;
            sync_crypto_directory(&self.parent)?;
        }
        Ok(())
    }

    fn file_mut(&mut self) -> std::io::Result<&mut std::fs::File> {
        self.file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("crypto staging handle unavailable"))
    }

    fn publish(&mut self) -> ShareResult<()> {
        self.file_mut()?.sync_all()?;
        self.validate_parent()?;
        let staged = crypto_file_witness(self.file_mut()?)?;
        if staged.state.links != 1
            || !staged.matches(
                &crypto_named_witness(&self.stage, OsStr::new("payload"))?,
                0,
            )
        {
            return Err(crypto_refusal("crypto staging identity or link topology changed").into());
        }
        // In particular, no shadowed witness may retain the replacement inode
        // when Windows later opens it exclusively for ReplaceFile.
        drop(staged);
        if let Some(expected) = &self.expected {
            let current = crypto_named_witness(&self.parent, &self.name)?;
            if !expected.matches(&current, 0) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "signature output changed before publication",
                )
                .into());
            }
            // Keep an independent name for the complete candidate before an
            // exchange can displace either its payload name or final name.
            self.stage.hard_link("payload", &self.stage, "candidate")?;
        }
        let mut staged = crypto_file_witness(self.file_mut()?)?;
        #[cfg(windows)]
        if self.expected.is_some() {
            // Windows settles last-write time only after the last writer closes.
            // Close both writable handles before capturing the comparison used
            // across ReplaceFile; the private hardlink pins the candidate ID.
            drop(self.file.take());
            drop(staged.retained.take());
            let file = open_crypto_leaf(&self.stage, OsStr::new("candidate"))?;
            let settled = crypto_file_witness(&file)?;
            staged.state.modified = settled.state.modified;
            if !staged.matches(&settled, 0) {
                return Err(crypto_refusal(
                    "crypto staging changed while settling its write handle",
                )
                .into());
            }
            staged = settled;
            self.file = Some(file);
        }
        if staged.state.links != if self.expected.is_some() { 2 } else { 1 } {
            return Err(crypto_refusal("crypto staging link topology changed").into());
        }
        #[cfg(test)]
        crypto_publication_hook(
            CryptoPublicationPhase::BeforePublish,
            &self.output,
            &self.stage_path.join("payload"),
        );
        if self.expected.is_some() {
            self.replace_expected(&mut staged)?;
        } else {
            // A single kernel operation decides the winner. Existing regular
            // files and dangling symlinks are never replaced or followed.
            self.stage.hard_link("payload", &self.parent, &self.name)?;
        }
        self.file_mut()?
            .sync_all()
            .map_err(|_| self.unconfirmed())?;
        self.sync_directories().map_err(|_| self.unconfirmed())?;
        self.validate_parent().map_err(|_| self.unconfirmed())?;
        let observed =
            crypto_named_witness(&self.parent, &self.name).map_err(|_| self.unconfirmed())?;
        if !staged.matches(&observed, u64::from(self.expected.is_none())) {
            return Err(self.unconfirmed().into());
        }
        Ok(())
    }

    #[cfg(any(windows, all(unix, any(target_vendor = "apple", target_os = "linux"))))]
    fn replace_expected(&mut self, staged: &mut CryptoFileWitness) -> std::io::Result<()> {
        #[cfg(unix)]
        let displaced = {
            rustix::fs::renameat_with(
                &self.stage,
                "payload",
                &self.parent,
                &self.name,
                rustix::fs::RenameFlags::EXCHANGE,
            )
            .map_err(std::io::Error::from)?;
            "payload"
        };
        #[cfg(windows)]
        let displaced = {
            let replaced = crypto_windows_path(&self.output)?;
            let replacement = crypto_windows_path(&self.stage_path.join("payload"))?;
            let backup = crypto_windows_path(&self.stage_path.join("replaced"))?;
            // ReplaceFile opens the replacement without sharing. The candidate
            // hardlink keeps its file ID allocated while these handles close.
            drop(self.file.take());
            drop(staged.retained.take());
            match winsafe::ReplaceFile(
                &replaced,
                &replacement,
                Some(&backup),
                winsafe::co::REPLACEFILE::default(),
            ) {
                Ok(()) => {}
                Err(error) if error == winsafe::co::ERROR::UNABLE_TO_MOVE_REPLACEMENT_2 => {
                    // Windows may have moved the old target into the backup
                    // before failing. Restore only into an absent final name.
                    self.move_retained_no_replace(false, "replaced")
                        .map_err(|_| self.unconfirmed())?;
                    return Err(std::io::Error::from_raw_os_error(error.raw().cast_signed()));
                }
                Err(error) => {
                    return Err(std::io::Error::from_raw_os_error(error.raw().cast_signed()));
                }
            }
            use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true).write(true).follow(FollowSymlinks::No);
            self.file = Some(
                self.stage
                    .open_with("candidate", &options)
                    .map_err(|_| self.unconfirmed())?
                    .into_std(),
            );
            "replaced"
        };
        let observed = crypto_named_witness(&self.stage, OsStr::new(displaced));
        if observed.as_ref().is_ok_and(|observed| {
            self.expected
                .as_ref()
                .is_some_and(|expected| expected.matches(observed, 0))
        }) {
            return Ok(());
        }

        // Never blindly exchange back: a third writer could have replaced the
        // final name after our exchange. Detach it without replacement, inspect
        // what was actually detached, and restore only to an absent final name.
        self.move_retained_no_replace(true, "rejected")
            .map_err(|_| self.unconfirmed())?;
        let detached = crypto_named_witness(&self.stage, OsStr::new("rejected"));
        if detached
            .as_ref()
            .is_ok_and(|detached| staged.matches(detached, 0))
        {
            self.move_retained_no_replace(false, displaced)
                .map_err(|_| self.unconfirmed())?;
            self.sync_directories().map_err(|_| self.unconfirmed())?;
            self.validate_parent().map_err(|_| self.unconfirmed())?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "signature output changed during publication; raced output restored and candidate retained",
            ));
        }
        self.move_retained_no_replace(false, "rejected")
            .map_err(|_| self.unconfirmed())?;
        self.sync_directories().map_err(|_| self.unconfirmed())?;
        Err(self.unconfirmed())
    }

    #[cfg(not(any(windows, all(unix, any(target_vendor = "apple", target_os = "linux")))))]
    fn replace_expected(&mut self, _staged: &mut CryptoFileWitness) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic signature replacement is unavailable on this platform; candidate retained",
        ))
    }

    #[cfg(any(windows, all(unix, any(target_vendor = "apple", target_os = "linux"))))]
    fn move_retained_no_replace(&self, from_output: bool, retained: &str) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let (source, source_name, destination, destination_name) = if from_output {
                (
                    &self.parent,
                    self.name.as_os_str(),
                    &self.stage,
                    OsStr::new(retained),
                )
            } else {
                (
                    &self.stage,
                    OsStr::new(retained),
                    &self.parent,
                    self.name.as_os_str(),
                )
            };
            rustix::fs::renameat_with(
                source,
                source_name,
                destination,
                destination_name,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(std::io::Error::from)
        }
        #[cfg(windows)]
        {
            let retained = self.stage_path.join(retained);
            let (source, destination) = if from_output {
                (&self.output, &retained)
            } else {
                (&retained, &self.output)
            };
            mcp_agent_mail_core::disk::windows_rename_noreplace(source, destination)
        }
    }
}

#[cfg(windows)]
fn crypto_windows_path(path: &Path) -> std::io::Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| crypto_refusal("Windows crypto publication requires a Unicode path"))?;
    if path.contains('\0') {
        return Err(crypto_refusal(
            "Windows crypto publication path must not contain a NUL byte",
        ));
    }
    Ok(path.to_owned())
}

fn require_real_crypto_file(path: &Path, label: &str) -> ShareResult<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ShareError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{label} must not be a symlink: {}", path.display()),
                )));
            }
            if !metadata.file_type().is_file() {
                return Err(ShareError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{label} must be a regular file: {}", path.display()),
                )));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(ShareError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{label} not found: {}", path.display()),
            )))
        }
        Err(error) => Err(ShareError::Io(error)),
    }
}

fn check_age_available() -> ShareResult<()> {
    match age_command().arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(ShareError::Io(std::io::Error::other(format!(
            "age CLI --version failed ({})",
            output.status
        )))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(ShareError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "age CLI not found in PATH. Install from https://github.com/FiloSottile/age",
            )))
        }
        Err(error) => Err(ShareError::Io(error)),
    }
}

fn age_command() -> std::process::Command {
    #[cfg(test)]
    {
        if let Some(path) = AGE_COMMAND_OVERRIDE.with(|slot| slot.borrow().clone()) {
            return std::process::Command::new(path);
        }
    }
    std::process::Command::new("age")
}

#[cfg(test)]
fn with_age_command_override<R>(path: &Path, f: impl FnOnce() -> R) -> R {
    struct AgeCommandOverrideGuard {
        previous: Option<PathBuf>,
    }

    impl Drop for AgeCommandOverrideGuard {
        fn drop(&mut self) {
            AGE_COMMAND_OVERRIDE.with(|slot| {
                let _ = slot.replace(self.previous.take());
            });
        }
    }

    AGE_COMMAND_OVERRIDE.with(|slot| {
        let previous = slot.replace(Some(path.to_path_buf()));
        let guard = AgeCommandOverrideGuard { previous };
        let result = f();
        drop(guard);
        result
    })
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

    #[cfg(unix)]
    #[test]
    fn crypto_directory_sync_accepts_retained_nofollow_handles() {
        use cap_fs_ext::DirExt as _;

        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("original-parent");
        let moved = root.path().join("moved-parent");
        std::fs::create_dir(&parent).unwrap();
        let root_authority =
            cap_std::fs::Dir::open_ambient_dir(root.path(), cap_std::ambient_authority()).unwrap();
        let retained = root_authority.open_dir_nofollow("original-parent").unwrap();

        // These are the exact authority handles used for publication, not
        // ordinary std::fs::File directory descriptors. Fsyncing a clone of
        // the authority itself fails with EBADF on Linux.
        sync_crypto_directory(&root_authority).unwrap();
        sync_crypto_directory(&retained).unwrap();

        std::fs::rename(&parent, &moved).unwrap();
        std::fs::create_dir(&parent).unwrap();
        sync_crypto_directory(&retained).unwrap();
        assert!(
            same_file::Handle::from_file(retained.into_std_file()).unwrap()
                == same_file::Handle::from_path(&moved).unwrap(),
            "directory synchronization must retain the original authority"
        );
    }

    #[test]
    fn sign_publishes_into_new_nested_parent_directories() {
        let root = tempfile::tempdir().unwrap();
        let manifest_bytes = br#"{"nested_parent":"durable signature"}"#;
        let manifest = root.path().join("manifest.json");
        let key = root.path().join("signing.key");
        std::fs::write(&manifest, manifest_bytes).unwrap();
        std::fs::write(&key, test_key_bytes()).unwrap();
        let parent = root.path().join("new").join("nested").join("bundle");
        let output = parent.join("manifest.sig.json");
        assert!(!parent.exists());

        let signed = sign_manifest(&manifest, &key, &output, false).unwrap();
        assert!(output.is_file());
        assert_eq!(std::fs::read(&manifest).unwrap(), manifest_bytes);
        std::fs::write(parent.join("manifest.json"), manifest_bytes).unwrap();
        let verified = verify_bundle(&parent, Some(&signed.public_key)).unwrap();
        assert!(verified.signature_checked && verified.signature_verified);
    }

    fn real_age_dependency_available(tool: &str, result: ShareResult<()>) -> bool {
        match result {
            Ok(()) => true,
            Err(ShareError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound
                    && std::env::var("AM_SHARE_REQUIRE_AGE").is_ok_and(|value| value == "0") =>
            {
                eprintln!(
                    "SKIP: {tool} is absent; optional local lane explicitly selected with AM_SHARE_REQUIRE_AGE=0"
                );
                false
            }
            Err(error) => panic!("required real-age dependency {tool} failed: {error}"),
        }
    }

    fn try_generate_age_identity(dir: &std::path::Path) -> Option<(std::path::PathBuf, String)> {
        if !real_age_dependency_available("age", check_age_available()) {
            return None;
        }
        let keygen_version = std::process::Command::new("age-keygen")
            .arg("--version")
            .output()
            .map_err(ShareError::Io)
            .and_then(|output| {
                if output.status.success() {
                    Ok(())
                } else {
                    Err(ShareError::Io(std::io::Error::other(format!(
                        "age-keygen --version failed ({})",
                        output.status
                    ))))
                }
            });
        if !real_age_dependency_available("age-keygen", keygen_version) {
            return None;
        }

        let identity_path = dir.join("age_identity.txt");
        let output = std::process::Command::new("age-keygen")
            .arg("-o")
            .arg(&identity_path)
            .output()
            .expect("age-keygen must run after its dependency probe succeeded");
        assert!(
            output.status.success(),
            "real age identity generation failed ({})",
            output.status
        );
        // Ask the real tool for the public recipient. Never print keygen's raw
        // output, which might contain identity material on an unexpected path.
        let output = std::process::Command::new("age-keygen")
            .arg("-y")
            .arg(&identity_path)
            .output()
            .expect("age-keygen must derive the generated identity's public recipient");
        assert!(
            output.status.success(),
            "real age public-recipient derivation failed ({})",
            output.status
        );
        let recipient = String::from_utf8(output.stdout)
            .unwrap_or_else(|_| panic!("age-keygen returned a non-UTF-8 public recipient"))
            .trim()
            .to_owned();
        assert!(
            recipient.parse::<age::x25519::Recipient>().is_ok(),
            "age-keygen returned an invalid public recipient"
        );
        Some((identity_path, recipient))
    }

    #[cfg(unix)]
    fn write_fake_age_script(dir: &Path, script_body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let script_path = dir.join("fake-age");
        std::fs::write(&script_path, script_body).unwrap();
        let mut permissions = std::fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script_path, permissions).unwrap();
        script_path
    }

    fn passphrase_ciphertext(plaintext: &[u8], passphrase: &str, armored: bool) -> Vec<u8> {
        use std::io::Write;

        let mut recipient =
            age::scrypt::Recipient::new(age::secrecy::SecretString::from(passphrase.to_owned()));
        // Unit fixtures use the real format/crypto with a cheap KDF. The CLI
        // interoperability lane separately encrypts with official age defaults.
        recipient.set_work_factor(10);
        let encryptor =
            age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
                .unwrap();
        let mut ciphertext = Vec::new();
        let mut writer = encryptor.wrap_output(&mut ciphertext).unwrap();
        writer.write_all(plaintext).unwrap();
        writer.finish().unwrap();
        if armored {
            let mut writer =
                age::armor::ArmoredWriter::wrap_output(Vec::new(), age::armor::Format::AsciiArmor)
                    .unwrap();
            writer.write_all(&ciphertext).unwrap();
            writer.finish().unwrap()
        } else {
            ciphertext
        }
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

    const PRIVACY_RAW_MARKERS: &[&str] = &[
        "/home/ubuntu/private/acme-client",
        "sk-abcdef0123456789012345",
        "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
        "Bearer dGVzdF90b2tlbl9uZXN0ZWQxMjM0NTY3ODkw",
        "PeerAgent",
    ];

    fn assert_text_omits_privacy_markers(label: &str, text: &str) {
        for marker in PRIVACY_RAW_MARKERS {
            assert!(
                !text.contains(marker),
                "{label} leaked private privacy-corpus marker: {marker}"
            );
        }
    }

    fn assert_bytes_omit_privacy_markers(label: &str, bytes: &[u8]) {
        for marker in PRIVACY_RAW_MARKERS {
            let marker_bytes = marker.as_bytes();
            assert!(
                !bytes
                    .windows(marker_bytes.len())
                    .any(|window| window == marker_bytes),
                "{label} leaked private privacy-corpus marker: {marker}"
            );
        }
    }

    fn write_privacy_proof_bundle_fixture(bundle_dir: &Path) -> String {
        std::fs::create_dir_all(bundle_dir.join("viewer/vendor")).unwrap();
        std::fs::create_dir_all(bundle_dir.join("viewer/data")).unwrap();

        let db_bytes = b"redacted mailbox proof for br-lmcob.13";
        std::fs::write(bundle_dir.join("mailbox.sqlite3"), db_bytes).unwrap();
        std::fs::write(
            bundle_dir.join("index.html"),
            r#"<!doctype html><meta name="agent-mail-schema" content="0.1.0"><main data-fixture="br-lmcob.13" data-product="prod-public">[project path redacted: acme-client]</main>"#,
        )
        .unwrap();
        std::fs::write(
            bundle_dir.join("viewer/index.html"),
            r#"<main data-fixture="br-lmcob.13">privacy proof viewer</main>"#,
        )
        .unwrap();
        std::fs::write(
            bundle_dir.join("viewer/vendor/app.js"),
            br#"window.AGENT_MAIL_PRIVACY_FIXTURE="br-lmcob.13";"#,
        )
        .unwrap();
        std::fs::write(
            bundle_dir.join("viewer/data/messages.json"),
            r#"[{"id":1,"subject":"Deploy [REDACTED]","body":"[Message body redacted]","project":"acme-client","product_uid":"prod-public"}]"#,
        )
        .unwrap();
        std::fs::write(
            bundle_dir.join("viewer/data/meta.json"),
            r#"{"schema_version":"0.1.0","privacy_fixture":"br-lmcob.13","scrub_preset":"strict","product_uid":"prod-public"}"#,
        )
        .unwrap();

        let app_js = std::fs::read(bundle_dir.join("viewer/vendor/app.js")).unwrap();
        let viewer_index = std::fs::read(bundle_dir.join("viewer/index.html")).unwrap();
        let mut sri_entries = serde_json::Map::new();
        sri_entries.insert(
            "vendor/app.js".to_string(),
            serde_json::Value::String(format!("sha256-{}", base64_encode(&sha256_bytes(&app_js)))),
        );
        sri_entries.insert(
            "viewer/index.html".to_string(),
            serde_json::Value::String(format!(
                "sha256-{}",
                base64_encode(&sha256_bytes(&viewer_index))
            )),
        );

        let manifest = serde_json::json!({
            "schema_version": "0.1.0",
            "bundle_type": "privacy-proof",
            "privacy_fixture": {
                "id": "br-lmcob.13",
                "public_proof": {
                    "project_slug": "acme-client",
                    "project_human_key": "[project path redacted: acme-client]",
                    "product_uid": "prod-public",
                    "scrub_preset": "strict"
                }
            },
            "database": {
                "path": "mailbox.sqlite3",
                "size_bytes": db_bytes.len(),
                "sha256": hex_sha256(db_bytes),
            },
            "viewer": {
                "sri": sri_entries,
                "data": {
                    "messages": "viewer/data/messages.json",
                    "meta": "viewer/data/meta.json"
                }
            },
        });
        let manifest_path = bundle_dir.join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let key_path = bundle_dir
            .parent()
            .unwrap_or(bundle_dir)
            .join("privacy_fixture_signing.key");
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
    fn tampered_database_fails_verification_despite_valid_signature() {
        let dir = tempfile::tempdir().unwrap();
        let public_key = write_signed_bundle_fixture(dir.path(), "full");

        // A pristine bundle verifies end-to-end, including the database payload.
        let ok = verify_bundle(dir.path(), Some(&public_key)).unwrap();
        assert!(ok.signature_verified, "pristine signature should verify");
        assert!(ok.database_checked, "database should have been checked");
        assert!(ok.database_verified, "pristine database should verify");
        assert!(ok.error.is_none());

        // Swap the database for attacker-controlled content WITHOUT touching the
        // signed manifest. The Ed25519 signature still validates (it only covers
        // manifest.json), but the database no longer matches the signed hash — the
        // exact gap this check closes.
        std::fs::write(
            dir.path().join("mailbox.sqlite3"),
            b"attacker-controlled database",
        )
        .unwrap();

        let tampered = verify_bundle(dir.path(), Some(&public_key)).unwrap();
        assert!(
            tampered.signature_verified,
            "manifest signature is untouched, so it still verifies"
        );
        assert!(tampered.database_checked);
        assert!(
            !tampered.database_verified,
            "a swapped database must fail verification"
        );
        assert!(
            tampered.error.is_some(),
            "a database mismatch must surface an error"
        );
    }

    #[test]
    fn chunked_bundle_verifies_both_chunks_and_standalone_database() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // A small "database" split into two chunks; the standalone file is the
        // concatenation, and the signed manifest hash is over that concatenation
        // (exactly how the exporter computes db_sha256, then chunks).
        let part0 = b"chunk-zero-contents-".to_vec();
        let part1 = b"chunk-one-contents!!".to_vec();
        let db_bytes: Vec<u8> = part0.iter().chain(part1.iter()).copied().collect();
        let db_hash = hex_sha256(&db_bytes);

        std::fs::create_dir_all(root.join("chunks")).unwrap();
        std::fs::write(root.join("chunks/00000.bin"), &part0).unwrap();
        std::fs::write(root.join("chunks/00001.bin"), &part1).unwrap();
        std::fs::write(root.join("mailbox.sqlite3"), &db_bytes).unwrap();

        let manifest = serde_json::json!({
            "schema_version": "0.1.0",
            "database": {
                "path": "mailbox.sqlite3",
                "size_bytes": db_bytes.len(),
                "sha256": db_hash,
                "chunked": true,
                "chunk_manifest": { "version": 1, "chunk_count": 2 },
            },
        });
        let manifest_path = root.join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let key_path = root.join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();
        let sig_path = root.join("manifest.sig.json");
        let public_key = sign_manifest(&manifest_path, &key_path, &sig_path, false)
            .unwrap()
            .public_key;

        // Pristine: both the reassembled chunks and the standalone DB match.
        let ok = verify_bundle(root, Some(&public_key)).unwrap();
        assert!(ok.signature_verified);
        assert!(
            ok.database_checked && ok.database_verified,
            "pristine chunked bundle should verify"
        );
        assert!(ok.error.is_none());

        // Swap the standalone mailbox.sqlite3 (chunks left intact). A consumer
        // that loads the standalone DB would otherwise get attacker content while
        // verify reported green — so this MUST fail.
        std::fs::write(
            root.join("mailbox.sqlite3"),
            b"attacker-controlled standalone db",
        )
        .unwrap();
        let swapped_db = verify_bundle(root, Some(&public_key)).unwrap();
        assert!(
            swapped_db.signature_verified,
            "manifest signature untouched"
        );
        assert!(
            !swapped_db.database_verified,
            "a swapped standalone DB must fail verification even when chunks are intact"
        );
        assert!(swapped_db.error.is_some());

        // Restore the standalone DB and tamper a chunk instead — must also fail.
        std::fs::write(root.join("mailbox.sqlite3"), &db_bytes).unwrap();
        std::fs::write(root.join("chunks/00001.bin"), b"tampered-chunk").unwrap();
        let swapped_chunk = verify_bundle(root, Some(&public_key)).unwrap();
        assert!(
            !swapped_chunk.database_verified,
            "a tampered chunk must fail verification"
        );
        assert!(swapped_chunk.error.is_some());
    }

    #[test]
    fn verify_checks_every_file_the_manifest_lists_and_ignores_others() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("viewer")).unwrap();
        std::fs::write(root.join("viewer/app.js"), b"render()").unwrap();
        // Unlisted, like an operator's CNAME or the deploy history.
        std::fs::write(root.join("CNAME"), b"mail.example.org").unwrap();
        let write_manifest = |files: serde_json::Value| {
            let manifest = serde_json::json!({ "files": files });
            std::fs::write(root.join("manifest.json"), manifest.to_string()).unwrap();
        };

        write_manifest(serde_json::json!({ "viewer/app.js": hex_sha256(b"render()") }));
        let intact = verify_bundle(root, None).unwrap();
        assert!(intact.files_checked && intact.files_verified, "{intact:?}");
        assert!(intact.error.is_none());

        std::fs::write(root.join("viewer/app.js"), b"exfiltrate()").unwrap();
        let altered = verify_bundle(root, None).unwrap();
        assert!(altered.files_checked && !altered.files_verified);
        assert!(altered.error.unwrap_or_default().contains("viewer/app.js"));

        for (files, expected) in [
            (
                serde_json::json!({ "viewer/gone.js": hex_sha256(b"x") }),
                "missing",
            ),
            (
                serde_json::json!({ "../outside.js": hex_sha256(b"x") }),
                "path traversal",
            ),
            (serde_json::json!({ "viewer/app.js": 7 }), "not a string"),
            (serde_json::json!(["viewer/app.js"]), "must map"),
        ] {
            write_manifest(files);
            let result = verify_bundle(root, None).unwrap();
            assert!(!result.files_verified);
            let error = result.error.unwrap_or_default();
            assert!(error.contains(expected), "{expected}: {error}");
        }

        // A manifest exported before the map existed has nothing to check.
        std::fs::write(root.join("manifest.json"), "{}").unwrap();
        let legacy = verify_bundle(root, None).unwrap();
        assert!(!legacy.files_checked && legacy.error.is_none());
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

    #[cfg(unix)]
    #[test]
    fn sign_rejects_symlinked_manifest() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_manifest = dir.path().join("real-manifest.json");
        std::fs::write(&real_manifest, r#"{"test": true}"#).unwrap();
        let linked_manifest = dir.path().join("manifest.json");
        symlink(&real_manifest, &linked_manifest).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let result = sign_manifest(
            &linked_manifest,
            &key_path,
            &dir.path().join("sig.json"),
            false,
        );
        assert!(matches!(result, Err(ShareError::ManifestNotFound { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn sign_rejects_symlinked_signature_output() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"test": true}"#).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let real_sig = dir.path().join("real.sig.json");
        std::fs::write(&real_sig, "{}").unwrap();
        let linked_sig = dir.path().join("manifest.sig.json");
        symlink(&real_sig, &linked_sig).unwrap();

        let result = sign_manifest(&manifest_path, &key_path, &linked_sig, true);
        assert!(matches!(
            result,
            Err(ShareError::Io(error))
                if error.kind() == std::io::ErrorKind::InvalidInput
                    && error.to_string().contains("must not be a symlink")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn sign_rejects_symlinked_signing_key() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"test": true}"#).unwrap();

        let real_key = dir.path().join("real.key");
        std::fs::write(&real_key, test_key_bytes()).unwrap();
        let linked_key = dir.path().join("linked.key");
        symlink(&real_key, &linked_key).unwrap();

        let result = sign_manifest(
            &manifest_path,
            &linked_key,
            &dir.path().join("sig.json"),
            false,
        );
        assert!(matches!(
            result,
            Err(ShareError::Io(error))
                if error.kind() == std::io::ErrorKind::InvalidInput
                    && error.to_string().contains("must not be a symlink")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn sign_rejects_symlinked_signature_parent_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"test": true}"#).unwrap();

        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, test_key_bytes()).unwrap();

        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let linked_parent = dir.path().join("linked-parent");
        symlink(&outside, &linked_parent).unwrap();

        let result = sign_manifest(
            &manifest_path,
            &key_path,
            &linked_parent.join("manifest.sig.json"),
            false,
        );
        assert!(matches!(
            result,
            Err(ShareError::Io(error))
                if error.kind() == std::io::ErrorKind::InvalidInput
                    && error.to_string().contains("must not be a symlink")
        ));
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

    #[cfg(unix)]
    #[test]
    fn verify_rejects_symlinked_bundle_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("manifest.json"), r#"{"test": true}"#).unwrap();

        let linked = dir.path().join("linked-bundle");
        symlink(&bundle, &linked).unwrap();

        let result = verify_bundle(&linked, None);
        assert!(matches!(result, Err(ShareError::ManifestNotFound { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn verify_rejects_symlinked_signature_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, r#"{"test": true}"#).unwrap();

        let real_sig = dir.path().join("real.sig.json");
        std::fs::write(&real_sig, r#"{"signature":"fake"}"#).unwrap();
        symlink(&real_sig, dir.path().join("manifest.sig.json")).unwrap();

        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.signature_checked);
        assert!(!result.signature_verified);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("must not be a symlink")
        );
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

    #[cfg(unix)]
    #[test]
    fn verify_sri_viewer_fallback_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let vendor_dir = dir.path().join("viewer").join("vendor");
        std::fs::create_dir_all(&vendor_dir).unwrap();

        let escaped_target = outside.path().join("payload.js");
        std::fs::write(&escaped_target, b"console.log('outside bundle')").unwrap();
        symlink(&escaped_target, vendor_dir.join("payload.js")).unwrap();

        let expected_sri = format!(
            "sha256-{}",
            base64_encode(&sha256_bytes(b"console.log('outside bundle')"))
        );
        let manifest = serde_json::json!({
            "viewer": {
                "sri": {
                    "vendor/payload.js": expected_sri
                }
            }
        });
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();

        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.sri_checked);
        assert!(
            !result.sri_valid,
            "viewer-relative symlink escapes must fail verification"
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("must not be a symlink"),
            "expected symlink rejection, got {:?}",
            result.error
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_sri_rejects_symlinked_file_inside_bundle() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let vendor_dir = dir.path().join("viewer").join("vendor");
        std::fs::create_dir_all(&vendor_dir).unwrap();

        let real_js = vendor_dir.join("real.js");
        std::fs::write(&real_js, b"console.log('inside bundle')").unwrap();
        let linked_js = vendor_dir.join("app.js");
        symlink(&real_js, &linked_js).unwrap();

        let expected_sri = format!(
            "sha256-{}",
            base64_encode(&sha256_bytes(b"console.log('inside bundle')"))
        );
        let manifest = serde_json::json!({
            "viewer": {
                "sri": {
                    "vendor/app.js": expected_sri
                }
            }
        });
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();

        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.sri_checked);
        assert!(!result.sri_valid);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("must not be a symlink")
        );
    }

    #[test]
    fn verify_sri_rejects_non_file_entries() {
        let dir = tempfile::tempdir().unwrap();
        let vendor_dir = dir.path().join("viewer").join("vendor");
        std::fs::create_dir_all(vendor_dir.join("nested")).unwrap();

        let manifest = serde_json::json!({
            "viewer": {
                "sri": {
                    "vendor/nested": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                }
            }
        });
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();

        let result = verify_bundle(dir.path(), None).unwrap();
        assert!(result.sri_checked);
        assert!(!result.sri_valid);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("regular file")
        );
    }

    #[test]
    fn age_encrypt_decrypt_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
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

    #[test]
    fn age_availability_reports_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let error = with_age_command_override(&dir.path().join("missing-age"), || {
            check_age_available().unwrap_err()
        });
        assert!(matches!(
            error,
            ShareError::Io(ref error) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[cfg(unix)]
    #[test]
    fn age_availability_does_not_treat_failed_version_as_absence() {
        let dir = tempfile::tempdir().unwrap();
        let command = write_fake_age_script(
            dir.path(),
            "#!/bin/sh\nprintf 'private diagnostic must be suppressed' >&2\nexit 19\n",
        );
        let error = with_age_command_override(&command, || check_age_available().unwrap_err());
        assert!(matches!(
            error,
            ShareError::Io(ref error) if error.kind() == std::io::ErrorKind::Other
        ));
        assert!(error.to_string().contains("--version failed"));
        assert!(!error.to_string().contains("private diagnostic"));
    }

    #[cfg(unix)]
    #[test]
    fn age_availability_preserves_launch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("non-executable-age");
        std::fs::write(&command, "#!/bin/sh\nexit 0\n").unwrap();
        let error = with_age_command_override(&command, || check_age_available().unwrap_err());
        assert!(matches!(
            error,
            ShareError::Io(ref error) if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn age_passphrase_roundtrip_binary_and_armored_without_cli() {
        let dir = tempfile::tempdir().unwrap();
        let missing_cli = dir.path().join("no-age-executable");
        let passphrase = "  fixture passphrase with trailing whitespace\t ";
        let multiple_chunks: Vec<u8> = (0..150_000).map(|i| (i % 251) as u8).collect();

        for (case, plaintext) in [
            ("empty", &[][..]),
            ("multiple-chunks", &multiple_chunks[..]),
        ] {
            let original = dir.path().join(format!("original-{case}.bin"));
            std::fs::write(&original, plaintext).unwrap();
            for armored in [false, true] {
                let ciphertext = passphrase_ciphertext(plaintext, passphrase, armored);
                let encrypted = dir.path().join(format!("{case}-{armored}.age"));
                let output = dir.path().join(format!("{case}-{armored}.decrypted"));
                std::fs::write(&encrypted, &ciphertext).unwrap();

                with_age_command_override(&missing_cli, || {
                    decrypt_with_age(&encrypted, &output, None, Some(passphrase)).unwrap();
                });

                assert_eq!(std::fs::read(&output).unwrap(), plaintext);
                assert_eq!(std::fs::read(&original).unwrap(), plaintext);
                assert_eq!(std::fs::read(&encrypted).unwrap(), ciphertext);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(
                        std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                        0o600,
                        "published plaintext must remain private"
                    );
                }
            }
        }
    }

    #[test]
    fn age_passphrase_wrong_secret_and_late_damage_publish_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let passphrase = "correct fixture passphrase";
        let plaintext = vec![0xA5; 150_000];
        let ciphertext = passphrase_ciphertext(&plaintext, passphrase, false);
        let original = dir.path().join("original.bin");
        let encrypted = dir.path().join("original.age");
        std::fs::write(&original, &plaintext).unwrap();
        std::fs::write(&encrypted, &ciphertext).unwrap();

        let mut corrupted = ciphertext.clone();
        *corrupted.last_mut().unwrap() ^= 1;
        let truncated = ciphertext[..ciphertext.len() - 1].to_vec();
        let mut retained_stages = 0;
        for (case, bytes, secret) in [
            (
                "wrong-secret",
                ciphertext.clone(),
                "wrong fixture passphrase",
            ),
            ("late-corruption", corrupted, passphrase),
            ("late-truncation", truncated, passphrase),
        ] {
            let input = dir.path().join(format!("{case}.age"));
            let output = dir.path().join(format!("{case}.decrypted"));
            std::fs::write(&input, &bytes).unwrap();
            let error = decrypt_with_age(&input, &output, None, Some(secret)).unwrap_err();
            let diagnostic = error.to_string();
            assert!(diagnostic.contains("check passphrase and file integrity"));
            assert!(!diagnostic.contains(secret));
            assert!(!diagnostic.contains(passphrase));
            assert!(!output.exists());
            assert_eq!(std::fs::read(&input).unwrap(), bytes);
            assert_eq!(std::fs::read(&original).unwrap(), plaintext);
            assert_eq!(std::fs::read(&encrypted).unwrap(), ciphertext);
            // Rejected authenticated streams retain their private stage for
            // diagnosis; they must never publish partial plaintext publicly.
            let stages: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(Result::unwrap)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".age-decrypt.")
                })
                .collect();
            assert_eq!(stages.len(), retained_stages + 1);
            retained_stages = stages.len();
            for stage in stages {
                assert!(stage.file_type().unwrap().is_dir());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(
                        stage.metadata().unwrap().permissions().mode() & 0o777,
                        0o700
                    );
                    let payload = stage.path().join("payload");
                    assert_eq!(
                        std::fs::metadata(payload).unwrap().permissions().mode() & 0o777,
                        0o600
                    );
                }
            }
        }
    }

    #[test]
    fn age_passphrase_refuses_existing_output_without_modifying_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let passphrase = "fixture passphrase";
        let ciphertext = passphrase_ciphertext(b"original plaintext", passphrase, false);
        let encrypted = dir.path().join("original.age");
        let output = dir.path().join("existing.bin");
        std::fs::write(&encrypted, &ciphertext).unwrap();
        std::fs::write(&output, b"keep existing output").unwrap();

        let error = decrypt_with_age(&encrypted, &output, None, Some(passphrase)).unwrap_err();
        assert!(matches!(
            error,
            ShareError::Io(error) if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(std::fs::read(&output).unwrap(), b"keep existing output");
        assert_eq!(std::fs::read(&encrypted).unwrap(), ciphertext);
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
        std::fs::write(&manifest_path, r#"{"test":"wrong key"}"#).unwrap();

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
        let mut corrupted = sig_bytes;
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
        assert_ne!(sig.signature, "");
        assert_ne!(sig.manifest_sha256, "");
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
        let dir = tempfile::tempdir().unwrap();
        let Some((_identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
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
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
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
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
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
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
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
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
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
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
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
    fn age_roundtrip_privacy_proof_bundle_preserves_redacted_replay_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
            return;
        };

        let source = dir.path().join("privacy_bundle_source");
        std::fs::create_dir_all(&source).unwrap();
        let explicit_pubkey = write_privacy_proof_bundle_fixture(&source);

        let exported_text = [
            "manifest.json",
            "index.html",
            "viewer/index.html",
            "viewer/data/messages.json",
            "viewer/data/meta.json",
        ]
        .iter()
        .map(|relative| std::fs::read_to_string(source.join(relative)).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
        assert_text_omits_privacy_markers("privacy proof source bundle", &exported_text);
        for public_marker in [
            "br-lmcob.13",
            "prod-public",
            "[project path redacted: acme-client]",
            "\"schema_version\": \"0.1.0\"",
        ] {
            assert!(
                exported_text.contains(public_marker),
                "privacy proof source bundle should retain public marker: {public_marker}"
            );
        }

        let zip_path = dir.path().join("privacy_bundle.zip");
        crate::package_directory_as_zip(&source, &zip_path).unwrap();
        let original_zip = std::fs::read(&zip_path).unwrap();
        assert_bytes_omit_privacy_markers("privacy proof zip", &original_zip);

        let encrypted = encrypt_with_age(&zip_path, std::slice::from_ref(&recipient)).unwrap();
        let ciphertext = std::fs::read(&encrypted).unwrap();
        assert_bytes_omit_privacy_markers("privacy proof age payload", &ciphertext);
        for public_marker in [
            b"br-lmcob.13",
            b"prod-public".as_slice(),
            b"[project path redacted: acme-client]".as_slice(),
        ] {
            assert!(
                !ciphertext
                    .windows(public_marker.len())
                    .any(|window| window == public_marker),
                "age ciphertext should not expose plaintext marker"
            );
        }

        let decrypted_zip_path = dir.path().join("privacy_bundle.decrypted.zip");
        decrypt_with_age(&encrypted, &decrypted_zip_path, Some(&identity_path), None).unwrap();
        let decrypted_zip = std::fs::read(&decrypted_zip_path).unwrap();
        assert_eq!(
            decrypted_zip, original_zip,
            "decrypted privacy proof bundle should match original zip bytes"
        );

        let extracted = dir.path().join("privacy_bundle_extracted");
        std::fs::create_dir_all(&extracted).unwrap();
        extract_zip_archive(&decrypted_zip_path, &extracted);

        let manifest_text = std::fs::read_to_string(extracted.join("manifest.json")).unwrap();
        assert_text_omits_privacy_markers("decrypted privacy manifest", &manifest_text);
        let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
        assert_eq!(
            manifest.get("schema_version").and_then(|v| v.as_str()),
            Some("0.1.0")
        );
        assert_eq!(
            manifest["privacy_fixture"]["public_proof"]["product_uid"],
            "prod-public"
        );
        assert_eq!(
            manifest["privacy_fixture"]["public_proof"]["project_human_key"],
            "[project path redacted: acme-client]"
        );

        let decrypted_exported_text = [
            "index.html",
            "viewer/index.html",
            "viewer/data/messages.json",
            "viewer/data/meta.json",
        ]
        .iter()
        .map(|relative| std::fs::read_to_string(extracted.join(relative)).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
        assert_text_omits_privacy_markers(
            "decrypted privacy static artifacts",
            &decrypted_exported_text,
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
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
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
        let dir = tempfile::tempdir().unwrap();

        // Generate two separate identities
        let id_dir_a = dir.path().join("id_a");
        std::fs::create_dir_all(&id_dir_a).unwrap();
        let Some((_identity_a, recipient_a)) = try_generate_age_identity(&id_dir_a) else {
            return;
        };

        let id_dir_b = dir.path().join("id_b");
        std::fs::create_dir_all(&id_dir_b).unwrap();
        let Some((identity_b, _recipient_b)) = try_generate_age_identity(&id_dir_b) else {
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
        let dir = tempfile::tempdir().unwrap();
        let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
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
    fn age_encrypt_refuses_existing_output() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("bundle.zip");
        let encrypted = dir.path().join("bundle.zip.age");
        std::fs::write(&input, b"data").unwrap();
        std::fs::write(&encrypted, b"existing ciphertext").unwrap();

        let err = encrypt_with_age(&input, &["age1example".to_string()])
            .expect_err("existing encrypted output should be rejected before encryption");
        assert!(format!("{err}").contains("already exists"));
    }

    #[cfg(unix)]
    #[test]
    fn age_encrypt_rejects_symlinked_input() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_input = dir.path().join("real.bin");
        std::fs::write(&real_input, b"data").unwrap();
        let linked_input = dir.path().join("linked.bin");
        symlink(&real_input, &linked_input).unwrap();

        let err = encrypt_with_age(&linked_input, &["age1example".to_string()])
            .expect_err("symlinked encryption inputs must be rejected");
        assert!(format!("{err}").contains("must not be a symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn age_encrypt_rejects_symlinked_output_path() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("bundle.zip");
        std::fs::write(&input, b"data").unwrap();

        let outside = dir.path().join("outside.age");
        std::fs::write(&outside, b"keep").unwrap();
        symlink(&outside, dir.path().join("bundle.zip.age")).unwrap();

        let err = encrypt_with_age(&input, &["age1example".to_string()])
            .expect_err("symlinked encrypted outputs must be rejected");
        assert!(format!("{err}").contains("must not be a symlink"));
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn age_encrypt_failure_does_not_leave_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("bundle.zip");
        std::fs::write(&input, b"data").unwrap();
        let script_path = write_fake_age_script(
            dir.path(),
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo \"fake age 1.0\"\n  exit 0\nfi\nprintf 'partial-ciphertext'\necho 'simulated encryption failure' >&2\nexit 1\n",
        );

        let err = with_age_command_override(&script_path, || {
            encrypt_with_age(&input, &["age1example".to_string()])
                .expect_err("fake age failure should surface")
        });
        assert!(format!("{err}").contains("check recipient and input"));
        assert!(!format!("{err}").contains("simulated encryption failure"));
        assert!(
            !input.with_extension("zip.age").exists(),
            "failed encryption should not strand a partial output at the final path"
        );
        let stage = std::fs::read_dir(dir.path())
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".age-encrypt.")
            })
            .expect("failed child stage must be retained");
        assert_eq!(
            std::fs::read(stage.path().join("payload")).unwrap(),
            b"partial-ciphertext"
        );
        assert_eq!(std::fs::read(&input).unwrap(), b"data");
    }

    #[test]
    fn age_decrypt_refuses_existing_output() {
        let dir = tempfile::tempdir().unwrap();
        let encrypted = dir.path().join("test.age");
        let output = dir.path().join("out.bin");
        let id_path = dir.path().join("id.txt");
        std::fs::write(&encrypted, b"fake").unwrap();
        std::fs::write(&output, b"existing").unwrap();
        std::fs::write(&id_path, b"fake identity").unwrap();

        let err = decrypt_with_age(&encrypted, &output, Some(&id_path), None)
            .expect_err("existing decrypt outputs must be rejected");
        assert!(format!("{err}").contains("already exists"));
    }

    #[cfg(unix)]
    #[test]
    fn age_decrypt_failure_does_not_leave_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let encrypted = dir.path().join("bundle.zip.age");
        let identity = dir.path().join("identity.txt");
        let output = dir.path().join("bundle.zip");
        std::fs::write(&encrypted, b"ciphertext").unwrap();
        std::fs::write(&identity, b"identity").unwrap();
        let script_path = write_fake_age_script(
            dir.path(),
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo \"fake age 1.0\"\n  exit 0\nfi\nprintf 'partial-plaintext'\necho 'simulated decryption failure' >&2\nexit 1\n",
        );

        let err = with_age_command_override(&script_path, || {
            decrypt_with_age(&encrypted, &output, Some(&identity), None)
                .expect_err("fake age failure should surface")
        });
        assert!(format!("{err}").contains("check identity and file integrity"));
        assert!(!format!("{err}").contains("simulated decryption failure"));
        assert!(
            !output.exists(),
            "failed decryption should not strand a partial output at the final path"
        );
        let stage = std::fs::read_dir(dir.path())
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".age-decrypt.")
            })
            .expect("failed child stage must be retained");
        assert_eq!(
            std::fs::read(stage.path().join("payload")).unwrap(),
            b"partial-plaintext"
        );
        assert_eq!(std::fs::read(&encrypted).unwrap(), b"ciphertext");
        assert_eq!(std::fs::read(&identity).unwrap(), b"identity");
    }

    #[cfg(unix)]
    #[test]
    fn age_decrypt_rejects_symlinked_identity_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let encrypted = dir.path().join("test.age");
        let output = dir.path().join("out.bin");
        let real_identity = dir.path().join("identity.txt");
        let linked_identity = dir.path().join("identity-link.txt");
        std::fs::write(&encrypted, b"fake").unwrap();
        std::fs::write(&real_identity, b"fake identity").unwrap();
        symlink(&real_identity, &linked_identity).unwrap();

        let err = decrypt_with_age(&encrypted, &output, Some(&linked_identity), None)
            .expect_err("symlinked identity files must be rejected");
        assert!(format!("{err}").contains("must not be a symlink"));
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
        assert_eq!(decoded, [] as [u8; 0]);
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
        assert_ne!(sig.signature, "");
        assert_ne!(sig.manifest_sha256, "");
        assert_ne!(sig.public_key, "");
        assert_ne!(sig.generated_at, "");

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
        use std::rc::Rc;
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        let key_path = dir.path().join("test.key");
        let sig_path = dir.path().join("manifest.sig.json");

        std::fs::write(&key_path, test_key_bytes()).unwrap();

        // Sign first version
        std::fs::write(&manifest_path, r#"{"version": 1}"#).unwrap();
        sign_manifest(&manifest_path, &key_path, &sig_path, false).unwrap();
        let original_signature = PublicationFileWitness::capture(&sig_path);

        // Update manifest and re-sign with overwrite
        std::fs::write(&manifest_path, r#"{"version": 2}"#).unwrap();
        let staged = Rc::new(RefCell::new(None));
        let recorded = Rc::clone(&staged);
        let sig2 = with_crypto_publication_hook(
            move |phase, _, candidate| {
                if matches!(phase, CryptoPublicationPhase::BeforePublish) {
                    *recorded.borrow_mut() = Some((
                        candidate.to_path_buf(),
                        PublicationFileWitness::capture(candidate),
                    ));
                }
            },
            || sign_manifest(&manifest_path, &key_path, &sig_path, true),
        )
        .unwrap();
        let staged = staged.borrow();
        let (candidate_path, candidate) = staged.as_ref().expect("overwrite reached publication");
        candidate.assert_unchanged(&sig_path);
        let stage_dir = candidate_path.parent().unwrap();
        let retained_original = retained_publication_file(stage_dir, &original_signature.identity);
        original_signature.assert_unchanged(&retained_original);
        let retained_candidate = retained_publication_file(stage_dir, &candidate.identity);
        candidate.assert_unchanged(&retained_candidate);

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

    // br-kp1in.49: child shims establish publication interleavings, not age
    // interoperability. Existing real-age tests retain that separate gate.
    #[derive(Clone, Copy, Debug)]
    enum PublicationOperation {
        Sign,
        PassphraseDecrypt,
        #[cfg(unix)]
        EncryptChild,
        #[cfg(unix)]
        IdentityDecryptChild,
    }

    impl PublicationOperation {
        fn all() -> &'static [Self] {
            &[
                Self::Sign,
                Self::PassphraseDecrypt,
                #[cfg(unix)]
                Self::EncryptChild,
                #[cfg(unix)]
                Self::IdentityDecryptChild,
            ]
        }

        fn label(self) -> &'static str {
            match self {
                Self::Sign => "ed25519-sign",
                Self::PassphraseDecrypt => "passphrase-decrypt",
                #[cfg(unix)]
                Self::EncryptChild => "encrypt-child-shim",
                #[cfg(unix)]
                Self::IdentityDecryptChild => "identity-decrypt-child-shim",
            }
        }
    }

    const PUBLICATION_PASSPHRASE: &str = "synthetic publication fixture passphrase";

    #[derive(Clone)]
    struct PublicationFixture {
        operation: PublicationOperation,
        parent: PathBuf,
        input: PathBuf,
        output: PathBuf,
        key: PathBuf,
        expected: Vec<u8>,
        #[cfg(unix)]
        child: PathBuf,
    }

    impl PublicationFixture {
        fn new(root: &Path, operation: PublicationOperation) -> Self {
            let parent = root.join("outputs");
            std::fs::create_dir(&parent).unwrap();
            let mut input = root.join("input.bin");
            let mut output = parent.join("result.bin");
            let key = root.join("synthetic.key");
            std::fs::write(&key, test_key_bytes()).unwrap();
            let expected: Vec<u8> = (0..150_000).map(|index| (index % 251) as u8).collect();
            match operation {
                PublicationOperation::Sign => {
                    input = root.join("manifest.json");
                    output = parent.join("manifest.sig.json");
                    std::fs::write(&input, br#"{"synthetic_publication":true}"#).unwrap();
                }
                PublicationOperation::PassphraseDecrypt => {
                    std::fs::write(
                        &input,
                        passphrase_ciphertext(&expected, PUBLICATION_PASSPHRASE, false),
                    )
                    .unwrap();
                }
                #[cfg(unix)]
                PublicationOperation::EncryptChild => {
                    input = parent.join("input.bin");
                    output = parent.join("input.bin.age");
                    std::fs::write(&input, &expected).unwrap();
                }
                #[cfg(unix)]
                PublicationOperation::IdentityDecryptChild => {
                    std::fs::write(&input, &expected).unwrap();
                }
            }
            #[cfg(unix)]
            let child = write_fake_age_script(
                root,
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo 'publication I/O fixture'\n  exit 0\nfi\ncat\n",
            );
            Self {
                operation,
                parent,
                input,
                output,
                key,
                expected,
                #[cfg(unix)]
                child,
            }
        }

        fn execute(&self, overwrite: bool) -> ShareResult<()> {
            match self.operation {
                PublicationOperation::Sign => {
                    sign_manifest(&self.input, &self.key, &self.output, overwrite).map(|_| ())
                }
                PublicationOperation::PassphraseDecrypt => decrypt_with_age(
                    &self.input,
                    &self.output,
                    None,
                    Some(PUBLICATION_PASSPHRASE),
                ),
                #[cfg(unix)]
                PublicationOperation::EncryptChild => {
                    with_age_command_override(&self.child, || {
                        encrypt_with_age(&self.input, &["synthetic-recipient".to_string()])
                            .map(|_| ())
                    })
                }
                #[cfg(unix)]
                PublicationOperation::IdentityDecryptChild => {
                    with_age_command_override(&self.child, || {
                        decrypt_with_age(&self.input, &self.output, Some(&self.key), None)
                    })
                }
            }
        }

        fn assert_complete(&self, path: &Path) {
            let bytes = std::fs::read(path).unwrap();
            if matches!(self.operation, PublicationOperation::Sign) {
                let signature: ManifestSignature = serde_json::from_slice(&bytes).unwrap();
                let manifest = std::fs::read(&self.input).unwrap();
                let public_key =
                    ed25519_dalek::SigningKey::from_bytes(&test_key_bytes()).verifying_key();
                let raw_signature: [u8; 64] = base64_decode(&signature.signature)
                    .unwrap()
                    .try_into()
                    .unwrap();
                assert_eq!(signature.public_key, base64_encode(public_key.as_bytes()));
                assert_eq!(signature.manifest_sha256, hex_sha256(&manifest));
                public_key
                    .verify_strict(
                        &manifest,
                        &ed25519_dalek::Signature::from_bytes(&raw_signature),
                    )
                    .unwrap();
            } else {
                assert!(
                    bytes == self.expected,
                    "published or staged output is incomplete"
                );
            }
        }
    }

    #[cfg(not(windows))]
    type PublicationFileIdentity = same_file::Handle;
    #[cfg(windows)]
    type PublicationFileIdentity = (u64, u64);

    struct PublicationFileWitness {
        identity: PublicationFileIdentity,
        size: u64,
        sha256: String,
        metadata: serde_json::Value,
    }

    impl PublicationFileWitness {
        fn capture(path: &Path) -> Self {
            use std::io::Read;
            let mut file = std::fs::File::open(path).unwrap();
            let metadata = file.metadata().unwrap();
            assert!(metadata.is_file());
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            #[cfg(unix)]
            let identity_metadata = {
                use std::os::unix::fs::MetadataExt;
                serde_json::json!({"device": metadata.dev(), "inode": metadata.ino(), "mode": metadata.mode() & 0o777})
            };
            #[cfg(windows)]
            let capability_metadata = {
                let retained = cap_std::fs::File::from_std(file.try_clone().unwrap());
                retained.metadata().unwrap()
            };
            #[cfg(windows)]
            let identity_metadata = serde_json::json!({
                "device": cap_fs_ext::MetadataExt::dev(&capability_metadata),
                "inode": cap_fs_ext::MetadataExt::ino(&capability_metadata),
                "readonly": metadata.permissions().readonly()
            });
            #[cfg(not(any(unix, windows)))]
            let identity_metadata =
                serde_json::json!({"identity_check": "retained same_file handle"});
            #[cfg(not(windows))]
            let identity = same_file::Handle::from_file(file).unwrap();
            // ReplaceFile opens its replacement exclusively. Keep numeric file
            // identity on Windows so the test observer cannot block that API.
            #[cfg(windows)]
            let identity = (
                cap_fs_ext::MetadataExt::dev(&capability_metadata),
                cap_fs_ext::MetadataExt::ino(&capability_metadata),
            );
            Self {
                identity,
                size: metadata.len(),
                sha256: hex_sha256(&bytes),
                metadata: identity_metadata,
            }
        }

        fn assert_unchanged(&self, path: &Path) -> Self {
            let observed = Self::capture(path);
            assert!(self.identity == observed.identity, "file identity changed");
            assert_eq!(self.size, observed.size, "file length changed");
            assert_eq!(self.sha256, observed.sha256, "file content changed");
            assert_eq!(self.metadata, observed.metadata, "file metadata changed");
            observed
        }

        fn json(&self) -> serde_json::Value {
            serde_json::json!({"size": self.size, "sha256": self.sha256, "metadata": self.metadata})
        }
    }

    fn retained_publication_fixture(case: &str, operation: PublicationOperation) -> PathBuf {
        let path = tempfile::Builder::new()
            .prefix(&format!("am-crypto-{case}-{}-", operation.label()))
            .tempdir()
            .unwrap()
            .keep();
        // Keep hook path comparisons stable across macOS /var and /tmp aliases.
        let path = path.canonicalize().unwrap();
        eprintln!("retained publication fixture: {}", path.display());
        path
    }

    fn write_publication_receipt(
        root: &Path,
        operation: PublicationOperation,
        details: serde_json::Value,
    ) {
        let receipt = serde_json::json!({
            "schema": "am-crypto-publication-unit-v1",
            "operation": operation.label(),
            "public_call_completed": true,
            "claim": "causal public API publication boundary; real-age interoperability is a separate lane",
            "details": details
        });
        std::fs::write(
            root.join("publication-receipt.json"),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
    }

    fn assert_publication_collision(result: ShareResult<()>) {
        assert!(
            matches!(result, Err(ShareError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists),
            "raced no-replace destination must produce AlreadyExists"
        );
    }

    fn retained_publication_file(directory: &Path, identity: &PublicationFileIdentity) -> PathBuf {
        for entry in std::fs::read_dir(directory).unwrap().map(Result::unwrap) {
            if entry.file_type().unwrap().is_file() {
                let observed = PublicationFileWitness::capture(&entry.path());
                if observed.identity == *identity {
                    return entry.path();
                }
            }
        }
        panic!("file identity was not retained in the private stage");
    }

    #[test]
    fn crypto_publication_preserves_a_raced_regular_destination() {
        use std::rc::Rc;
        for &operation in PublicationOperation::all() {
            let modes: &[bool] = if matches!(operation, PublicationOperation::Sign) {
                &[false, true]
            } else {
                &[false]
            };
            for &overwrite in modes {
                let root = retained_publication_fixture("raced-regular", operation);
                let fixture = PublicationFixture::new(&root, operation);
                let source_before = PublicationFileWitness::capture(&fixture.input);
                let key_before = PublicationFileWitness::capture(&fixture.key);
                let witnessed = Rc::new(RefCell::new(None));
                let recorded = Rc::clone(&witnessed);
                let result = with_crypto_publication_hook(
                    move |phase, destination, staged| {
                        if matches!(phase, CryptoPublicationPhase::BeforePublish) {
                            assert!(!destination.exists());
                            let candidate = PublicationFileWitness::capture(staged);
                            std::fs::write(destination, b"concurrent writer owns these bytes\0")
                                .unwrap();
                            let competitor = PublicationFileWitness::capture(destination);
                            *recorded.borrow_mut() =
                                Some((staged.to_path_buf(), candidate, competitor));
                        }
                    },
                    || fixture.execute(overwrite),
                );
                assert_publication_collision(result);
                let witnessed = witnessed.borrow();
                let (staged, candidate, competitor) =
                    witnessed.as_ref().expect("publication seam was reached");
                let after = competitor.assert_unchanged(&fixture.output);
                candidate.assert_unchanged(staged);
                fixture.assert_complete(staged);
                let source_after = source_before.assert_unchanged(&fixture.input);
                key_before.assert_unchanged(&fixture.key);
                write_publication_receipt(
                    &root,
                    operation,
                    serde_json::json!({
                        "case": "raced-regular", "overwrite_flag": overwrite, "result": "AlreadyExists",
                        "output_parent": fixture.parent,
                        "destination_before": competitor.json(), "destination_after": after.json(),
                        "source_before": source_before.json(), "source_after": source_after.json(),
                        "key_identity_preserved": true, "staged_path": staged, "staged_candidate": candidate.json()
                    }),
                );
            }
        }
    }

    #[cfg(unix)]
    fn publication_symlink_witness(path: &Path) -> serde_json::Value {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path).unwrap();
        assert!(metadata.file_type().is_symlink());
        serde_json::json!({"device": metadata.dev(), "inode": metadata.ino(), "target": std::fs::read_link(path).unwrap()})
    }

    #[cfg(unix)]
    #[test]
    fn crypto_publication_preserves_raced_dangling_and_outside_symlinks() {
        use std::os::unix::fs::symlink;
        use std::rc::Rc;
        for &operation in PublicationOperation::all() {
            for dangling in [false, true] {
                let root = retained_publication_fixture("raced-symlink", operation);
                let fixture = PublicationFixture::new(&root, operation);
                let source_before = PublicationFileWitness::capture(&fixture.input);
                let key_before = PublicationFileWitness::capture(&fixture.key);
                let outside = root.join("outside-sentinel");
                std::fs::write(&outside, b"outside sentinel must survive").unwrap();
                let outside_before = PublicationFileWitness::capture(&outside);
                let target = if dangling {
                    root.join("absent-target")
                } else {
                    outside.clone()
                };
                let target_for_hook = target.clone();
                let witnessed = Rc::new(RefCell::new(None));
                let recorded = Rc::clone(&witnessed);
                let result = with_crypto_publication_hook(
                    move |phase, destination, staged| {
                        if matches!(phase, CryptoPublicationPhase::BeforePublish) {
                            let candidate = PublicationFileWitness::capture(staged);
                            symlink(&target_for_hook, destination).unwrap();
                            *recorded.borrow_mut() = Some((
                                staged.to_path_buf(),
                                candidate,
                                publication_symlink_witness(destination),
                            ));
                        }
                    },
                    || fixture.execute(false),
                );
                assert_publication_collision(result);
                let witnessed = witnessed.borrow();
                let (staged, candidate, link_before) =
                    witnessed.as_ref().expect("publication seam was reached");
                let link_after = publication_symlink_witness(&fixture.output);
                assert_eq!(*link_before, link_after);
                outside_before.assert_unchanged(&outside);
                if dangling {
                    assert!(!target.exists(), "a raced dangling symlink was followed");
                }
                candidate.assert_unchanged(staged);
                fixture.assert_complete(staged);
                let source_after = source_before.assert_unchanged(&fixture.input);
                key_before.assert_unchanged(&fixture.key);
                write_publication_receipt(
                    &root,
                    operation,
                    serde_json::json!({
                        "case": "raced-symlink", "dangling": dangling, "result": "AlreadyExists",
                        "link_before": link_before, "link_after": link_after,
                        "outside_sentinel": outside_before.json(), "key_identity_preserved": true,
                        "source_before": source_before.json(), "source_after": source_after.json(),
                        "staged_path": staged, "staged_candidate": candidate.json()
                    }),
                );
            }
        }
    }

    #[test]
    fn crypto_publication_concurrent_writers_have_one_complete_winner() {
        use std::sync::mpsc;
        use std::time::Duration;
        for &operation in PublicationOperation::all() {
            let root = retained_publication_fixture("two-writers", operation);
            let fixture = PublicationFixture::new(&root, operation);
            let source_before = PublicationFileWitness::capture(&fixture.input);
            let key_before = PublicationFileWitness::capture(&fixture.key);
            let (ready_send, ready_receive) = mpsc::channel();
            let (done_send, done_receive) = mpsc::channel();
            std::thread::scope(|scope| {
                let mut releases = Vec::new();
                for index in 0..2 {
                    let worker_fixture = fixture.clone();
                    let ready = ready_send.clone();
                    let done = done_send.clone();
                    let (release, resume) = mpsc::channel();
                    releases.push(release);
                    scope.spawn(move || {
                        let result = with_crypto_publication_hook(
                            move |phase, _, staged| {
                                if matches!(phase, CryptoPublicationPhase::BeforePublish) {
                                    ready.send((index, staged.to_path_buf())).unwrap();
                                    resume
                                        .recv_timeout(Duration::from_secs(15))
                                        .expect("bounded publication release");
                                }
                            },
                            || worker_fixture.execute(false),
                        );
                        done.send((index, result)).unwrap();
                    });
                }
                let mut stages = Vec::new();
                for _ in 0..2 {
                    let (index, path) = ready_receive
                        .recv_timeout(Duration::from_secs(15))
                        .expect("both writers must finish staging before publication");
                    let witness = PublicationFileWitness::capture(&path);
                    fixture.assert_complete(&path);
                    stages.push((index, path, witness));
                }
                assert!(
                    !fixture.output.exists(),
                    "no partial destination may be exposed while writers stage"
                );
                releases[0].send(()).unwrap();
                let (winner, result) = done_receive
                    .recv_timeout(Duration::from_secs(15))
                    .expect("winner completion");
                assert_eq!(winner, 0);
                result.expect("first writer must publish successfully");
                let winning_output = PublicationFileWitness::capture(&fixture.output);
                fixture.assert_complete(&fixture.output);
                releases[1].send(()).unwrap();
                let (loser, result) = done_receive
                    .recv_timeout(Duration::from_secs(15))
                    .expect("loser completion");
                assert_eq!(loser, 1);
                assert_publication_collision(result);
                let after = winning_output.assert_unchanged(&fixture.output);
                for (index, path, candidate) in &stages {
                    candidate.assert_unchanged(path);
                    if *index == winner {
                        assert!(
                            candidate.identity == winning_output.identity,
                            "published object must be the complete winning stage"
                        );
                    }
                }
                let source_after = source_before.assert_unchanged(&fixture.input);
                key_before.assert_unchanged(&fixture.key);
                write_publication_receipt(
                    &root,
                    operation,
                    serde_json::json!({
                        "case": "two-concurrent-writers", "winner": winner, "loser": loser,
                        "winner_result": "success", "loser_result": "AlreadyExists",
                        "both_producers_completed_before_publication": true,
                        "source_before": source_before.json(), "source_after": source_after.json(), "key_identity_preserved": true,
                        "winner_before_loser": winning_output.json(), "winner_after_loser": after.json(),
                        "retained_stages": stages.iter().map(|(index, path, witness)| serde_json::json!({"writer": index, "path": path, "witness": witness.json()})).collect::<Vec<_>>()
                    }),
                );
            });
        }
    }

    #[cfg(unix)]
    #[test]
    fn crypto_publication_parent_substitution_cannot_redirect_or_clean_up_unrelated_files() {
        use std::os::unix::fs::symlink;
        use std::rc::Rc;
        for &operation in PublicationOperation::all() {
            for late_swap in [false, true] {
                for substitute_symlink in [false, true] {
                    let root = retained_publication_fixture("parent-substitution", operation);
                    let fixture = PublicationFixture::new(&root, operation);
                    let source_before = PublicationFileWitness::capture(&fixture.input);
                    let key_before = PublicationFileWitness::capture(&fixture.key);
                    let retained_parent = root.join("retained-original-parent");
                    let substitute = root.join("unrelated-directory");
                    std::fs::create_dir(&substitute).unwrap();
                    let substitute_output = substitute.join(fixture.output.file_name().unwrap());
                    std::fs::write(&substitute_output, b"unrelated directory owner").unwrap();
                    let substitute_before = PublicationFileWitness::capture(&substitute_output);
                    let retained_for_hook = retained_parent.clone();
                    let substitute_for_hook = substitute.clone();
                    let parent_for_hook = fixture.parent.clone();
                    let witnessed = Rc::new(RefCell::new(None));
                    let recorded = Rc::clone(&witnessed);
                    let result = with_crypto_publication_hook(
                        move |phase, _, staged| {
                            let selected = if late_swap {
                                matches!(phase, CryptoPublicationPhase::BeforePublish)
                            } else {
                                matches!(phase, CryptoPublicationPhase::Captured)
                            };
                            if !selected {
                                return;
                            }
                            let staged_relative =
                                staged.strip_prefix(&parent_for_hook).unwrap().to_path_buf();
                            let candidate_before = PublicationFileWitness::capture(staged);
                            std::fs::rename(&parent_for_hook, &retained_for_hook).unwrap();
                            if substitute_symlink {
                                symlink(&substitute_for_hook, &parent_for_hook).unwrap();
                            } else {
                                std::fs::rename(&substitute_for_hook, &parent_for_hook).unwrap();
                            }
                            // A path-based TempDir destructor would delete this
                            // unrelated tree after the original parent was moved.
                            let trap = parent_for_hook.join(&staged_relative);
                            std::fs::create_dir_all(trap.parent().unwrap()).unwrap();
                            std::fs::write(&trap, b"unrelated staging-name cleanup trap").unwrap();
                            let trap_before = PublicationFileWitness::capture(&trap);
                            *recorded.borrow_mut() = Some((
                                retained_for_hook.join(staged_relative),
                                candidate_before,
                                trap,
                                trap_before,
                            ));
                        },
                        || fixture.execute(false),
                    );
                    assert!(
                        result.is_err(),
                        "changed output pathname authority must report publication unconfirmed"
                    );
                    let witnessed = witnessed.borrow();
                    let (staged, candidate_before, trap, trap_before) = witnessed
                        .as_ref()
                        .expect("parent substitution seam was reached");
                    substitute_before.assert_unchanged(&fixture.output);
                    trap_before.assert_unchanged(trap);
                    let candidate_after = PublicationFileWitness::capture(staged);
                    assert!(candidate_before.identity == candidate_after.identity);
                    if late_swap {
                        candidate_before.assert_unchanged(staged);
                    }
                    fixture.assert_complete(staged);
                    let source_after_path =
                        fixture.input.strip_prefix(&fixture.parent).map_or_else(
                            |_| fixture.input.clone(),
                            |relative| retained_parent.join(relative),
                        );
                    let source_after = source_before.assert_unchanged(&source_after_path);
                    key_before.assert_unchanged(&fixture.key);
                    let original_output = retained_parent.join(fixture.output.file_name().unwrap());
                    let original_published = original_output.exists();
                    if late_swap && original_published {
                        // A late namespace swap may leave a complete output in
                        // the retained original directory, but never success at
                        // the substituted requested pathname.
                        fixture.assert_complete(&original_output);
                    } else {
                        assert!(
                            !original_published,
                            "early parent substitution must refuse before publication"
                        );
                    }
                    write_publication_receipt(
                        &root,
                        operation,
                        serde_json::json!({
                            "case": "parent-substitution", "phase": if late_swap { "BeforePublish" } else { "Captured" },
                            "substitute_symlink": substitute_symlink, "result": "refused-or-publication-unconfirmed",
                            "substitute_output": substitute_before.json(), "cleanup_trap": trap_before.json(),
                            "retained_stage_path": staged, "retained_stage": candidate_after.json(),
                            "original_directory_has_complete_output": original_published,
                            "source_before": source_before.json(), "source_after": source_after.json(), "key_identity_preserved": true
                        }),
                    );
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn crypto_publication_windows_parent_authority_blocks_rebinding() {
        use std::rc::Rc;
        for &operation in PublicationOperation::all() {
            for late_swap in [false, true] {
                let root = retained_publication_fixture("windows-parent-authority", operation);
                let fixture = PublicationFixture::new(&root, operation);
                let moved_parent = root.join("renamed-parent");
                // The actor can rename this fixture before authority capture;
                // a later refusal must come from the retained directory handles.
                std::fs::rename(&fixture.parent, &moved_parent).unwrap();
                std::fs::rename(&moved_parent, &fixture.parent).unwrap();
                let source_before = PublicationFileWitness::capture(&fixture.input);
                let key_before = PublicationFileWitness::capture(&fixture.key);
                let moved_for_hook = moved_parent.clone();
                let witnessed = Rc::new(RefCell::new(None));
                let recorded = Rc::clone(&witnessed);
                with_crypto_publication_hook(
                    move |phase, destination, staged| {
                        let selected = if late_swap {
                            matches!(phase, CryptoPublicationPhase::BeforePublish)
                        } else {
                            matches!(phase, CryptoPublicationPhase::Captured)
                        };
                        if selected {
                            let error =
                                std::fs::rename(destination.parent().unwrap(), &moved_for_hook)
                                    .expect_err(
                                        "retained Windows directory authority must block rebinding",
                                    );
                            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                            *recorded.borrow_mut() =
                                Some((staged.to_path_buf(), error.raw_os_error()));
                        }
                    },
                    || fixture.execute(false),
                )
                .expect("refused parent rename must leave valid publication available");
                let witnessed = witnessed.borrow();
                let (stage_path, os_error) =
                    witnessed.as_ref().expect("parent rename was attempted");
                fixture.assert_complete(&fixture.output);
                let candidate = PublicationFileWitness::capture(stage_path);
                let published = candidate.assert_unchanged(&fixture.output);
                let source_after = source_before.assert_unchanged(&fixture.input);
                key_before.assert_unchanged(&fixture.key);
                // Once the public operation returns, its directory authority
                // is released; the fixture's positive rename control works again.
                std::fs::rename(&fixture.parent, &moved_parent).unwrap();
                std::fs::rename(&moved_parent, &fixture.parent).unwrap();
                write_publication_receipt(
                    &root,
                    operation,
                    serde_json::json!({
                        "case": "windows-parent-authority", "phase": if late_swap { "BeforePublish" } else { "Captured" },
                        "parent_rename_os_error": os_error, "public_result": "success",
                        "published": published.json(), "retained_stage": stage_path,
                        "source_before": source_before.json(), "source_after": source_after.json(), "key_identity_preserved": true
                    }),
                );
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum SignatureTargetMutation {
        ReplaceRegular,
        ChangeInPlace,
        #[cfg(unix)]
        ReplaceSymlink,
    }

    #[test]
    fn signature_overwrite_refuses_a_changed_authorized_target() {
        use std::rc::Rc;
        for mutation in [
            SignatureTargetMutation::ReplaceRegular,
            SignatureTargetMutation::ChangeInPlace,
            #[cfg(unix)]
            SignatureTargetMutation::ReplaceSymlink,
        ] {
            for late_change in [false, true] {
                let root = retained_publication_fixture(
                    "overwrite-changed-target",
                    PublicationOperation::Sign,
                );
                let fixture = PublicationFixture::new(&root, PublicationOperation::Sign);
                fixture.execute(false).unwrap();
                let old_signature = PublicationFileWitness::capture(&fixture.output);
                std::fs::write(
                    &fixture.input,
                    br#"{"synthetic_publication":true,"generation":2}"#,
                )
                .unwrap();
                let source_before = PublicationFileWitness::capture(&fixture.input);
                let key_before = PublicationFileWitness::capture(&fixture.key);
                let displaced = root.join("original-signature-retained-by-other-writer");
                let outside = root.join("outside-sentinel");
                std::fs::write(&outside, b"outside signature sentinel").unwrap();
                let outside_before = PublicationFileWitness::capture(&outside);
                let displaced_for_hook = displaced.clone();
                #[cfg(unix)]
                let outside_for_hook = outside.clone();
                let witnessed = Rc::new(RefCell::new(None));
                let recorded = Rc::clone(&witnessed);
                let result = with_crypto_publication_hook(
                    move |phase, destination, staged| {
                        let selected = if late_change {
                            matches!(phase, CryptoPublicationPhase::BeforePublish)
                        } else {
                            matches!(phase, CryptoPublicationPhase::Captured)
                        };
                        if !selected {
                            return;
                        }
                        let candidate = PublicationFileWitness::capture(staged);
                        let (regular, link): (
                            Option<PublicationFileWitness>,
                            Option<serde_json::Value>,
                        ) = match mutation {
                            SignatureTargetMutation::ReplaceRegular => {
                                std::fs::rename(destination, &displaced_for_hook).unwrap();
                                std::fs::write(destination, b"concurrently replaced signature")
                                    .unwrap();
                                (Some(PublicationFileWitness::capture(destination)), None)
                            }
                            SignatureTargetMutation::ChangeInPlace => {
                                std::fs::write(
                                    destination,
                                    b"concurrent same-inode signature edit",
                                )
                                .unwrap();
                                (Some(PublicationFileWitness::capture(destination)), None)
                            }
                            #[cfg(unix)]
                            SignatureTargetMutation::ReplaceSymlink => {
                                std::fs::rename(destination, &displaced_for_hook).unwrap();
                                std::os::unix::fs::symlink(&outside_for_hook, destination).unwrap();
                                (None, Some(publication_symlink_witness(destination)))
                            }
                        };
                        *recorded.borrow_mut() =
                            Some((staged.to_path_buf(), candidate, regular, link));
                    },
                    || fixture.execute(true),
                );
                assert!(
                    matches!(result, Err(ShareError::Io(_))),
                    "changed overwrite authority must be refused"
                );
                let witnessed = witnessed.borrow();
                let (staged, candidate, regular, link) = witnessed
                    .as_ref()
                    .expect("overwrite mutation seam was reached");
                if let Some(regular) = regular {
                    regular.assert_unchanged(&fixture.output);
                    if matches!(mutation, SignatureTargetMutation::ChangeInPlace) {
                        assert!(
                            old_signature.identity == regular.identity,
                            "in-place fixture must keep the captured inode"
                        );
                    }
                }
                #[cfg(unix)]
                if let Some(link) = link {
                    assert_eq!(*link, publication_symlink_witness(&fixture.output));
                }
                if !matches!(mutation, SignatureTargetMutation::ChangeInPlace) {
                    old_signature.assert_unchanged(&displaced);
                }
                outside_before.assert_unchanged(&outside);
                let source_after = source_before.assert_unchanged(&fixture.input);
                key_before.assert_unchanged(&fixture.key);
                let candidate_path =
                    retained_publication_file(staged.parent().unwrap(), &candidate.identity);
                if late_change {
                    candidate.assert_unchanged(&candidate_path);
                }
                fixture.assert_complete(&candidate_path);
                write_publication_receipt(
                    &root,
                    PublicationOperation::Sign,
                    serde_json::json!({
                        "case": "overwrite-changed-target", "mutation": format!("{mutation:?}"),
                        "phase": if late_change { "BeforePublish" } else { "Captured" }, "result": "refused",
                        "retained_attempted_signature": candidate_path,
                        "replacement_regular": regular.as_ref().map(PublicationFileWitness::json), "replacement_link": link,
                        "outside_sentinel": outside_before.json(),
                        "source_before": source_before.json(), "source_after": source_after.json(), "key_identity_preserved": true
                    }),
                );
            }
        }
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
