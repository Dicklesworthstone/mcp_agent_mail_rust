//! Signed release-manifest verification for the built-in self-updater (GH#292).
//!
//! Releases v0.3.31 and later publish a `SHA256SUMS` manifest together with a
//! detached minisign signature (`SHA256SUMS.minisig`) made with the
//! maintainer-held release key. The curl/PowerShell installers verify that
//! signature over the exact manifest bytes before trusting any archive hash.
//! `am self-update` fetches the manifest and the archive from the same release
//! location, so a bare SHA-256 comparison only proves the archive matches
//! whatever manifest that location served — it does not prove the release is
//! the maintainer's. This module closes that gap with a pure-Rust minisign
//! verifier (ed25519 + BLAKE2b-512), so no `minisign` executable is required.
//!
//! # Trust anchors and rotation
//!
//! [`TRUSTED_RELEASE_MINISIGN_KEYS`] is the pinned set of release public keys.
//! A signature names the key it was made with (an 8-byte key id embedded in
//! both the public key and the signature); verification selects the trusted
//! key with that id and fails closed if no trusted key has it. Rotating the
//! release key is therefore: ship a release whose updater trusts both the old
//! and the new key, sign subsequent releases with the new key, and drop the
//! old key from the list once no supported updater still needs it. A key that
//! is not in the list can never verify anything, so a compromised old key is
//! neutralised by removing it.
//!
//! # Test override
//!
//! Debug builds honour [`TRUSTED_KEYS_OVERRIDE_ENV`] so the E2E suite can sign
//! a mock release with a throwaway key. Release builds refuse to run the
//! updater when that variable is set, rather than silently ignoring it —
//! nothing outside the binary may widen the trust anchor of a shipped `am`.

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};

/// Minisign public keys allowed to sign release manifests, in the
/// `minisign.pub` one-line encoding (base64 of `"Ed" || key_id || pk`).
pub(crate) const TRUSTED_RELEASE_MINISIGN_KEYS: &[&str] = &[
    // Maintainer release key, id 1BBD79B28BF718D0. The same key is pinned in
    // install.sh (`MINISIGN_PUBLIC_KEY`) and install.ps1.
    "RWTQGPeLsnm9G7VFdFWkkcRi3wJK/PqsYxWC+oLNN74W9IjBxRU1Xu70",
];

/// Debug-build-only override of the trusted key set (whitespace- or
/// comma-separated minisign public keys). Ignored is not an option: release
/// builds fail closed when this is set.
pub(crate) const TRUSTED_KEYS_OVERRIDE_ENV: &str = "AM_SELF_UPDATE_MINISIGN_PUBKEY";

const SIG_ALG_LEGACY: &[u8; 2] = b"Ed";
const SIG_ALG_PREHASHED: &[u8; 2] = b"ED";
const KEY_ID_LEN: usize = 8;
const PUBLIC_KEY_LEN: usize = 2 + KEY_ID_LEN + 32;
const SIGNATURE_BLOB_LEN: usize = 2 + KEY_ID_LEN + 64;
const UNTRUSTED_COMMENT_PREFIX: &str = "untrusted comment:";
const TRUSTED_COMMENT_PREFIX: &str = "trusted comment: ";

/// A parsed minisign public key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MinisignPublicKey {
    key_id: [u8; KEY_ID_LEN],
    key: VerifyingKey,
}

impl MinisignPublicKey {
    /// Parse the one-line `minisign.pub` encoding (without the untrusted
    /// comment line).
    pub(crate) fn parse(encoded: &str) -> Result<Self, String> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|e| format!("minisign public key is not valid base64: {e}"))?;
        if raw.len() != PUBLIC_KEY_LEN {
            return Err(format!(
                "minisign public key must decode to {PUBLIC_KEY_LEN} bytes (got {})",
                raw.len()
            ));
        }
        if &raw[..2] != SIG_ALG_LEGACY {
            return Err("minisign public key has an unsupported algorithm tag".to_string());
        }
        let mut key_id = [0u8; KEY_ID_LEN];
        key_id.copy_from_slice(&raw[2..2 + KEY_ID_LEN]);
        let mut pk = [0u8; 32];
        pk.copy_from_slice(&raw[2 + KEY_ID_LEN..]);
        let key = VerifyingKey::from_bytes(&pk)
            .map_err(|e| format!("minisign public key is not a valid ed25519 key: {e}"))?;
        Ok(Self { key_id, key })
    }

    /// Key id rendered the way `minisign` prints it (little-endian u64, upper
    /// hex), e.g. `1BBD79B28BF718D0`.
    pub(crate) fn key_id_hex(&self) -> String {
        format_key_id(&self.key_id)
    }
}

fn format_key_id(key_id: &[u8; KEY_ID_LEN]) -> String {
    format!("{:X}", u64::from_le_bytes(*key_id))
}

/// A parsed `.minisig` file.
#[derive(Debug)]
struct MinisignSignature {
    prehashed: bool,
    key_id: [u8; KEY_ID_LEN],
    signature: Signature,
    trusted_comment: Vec<u8>,
    global_signature: Signature,
}

impl MinisignSignature {
    /// Parse the four-line `.minisig` format:
    ///
    /// ```text
    /// untrusted comment: <free text>
    /// <base64: sig_alg(2) || key_id(8) || signature(64)>
    /// trusted comment: <free text, covered by the global signature>
    /// <base64: global_signature(64) over signature || trusted comment>
    /// ```
    fn parse(raw: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(raw)
            .map_err(|_| "manifest signature file is not valid UTF-8".to_string())?;
        let mut lines = text.lines().map(|line| line.trim_end_matches('\r'));
        let untrusted = lines
            .next()
            .ok_or_else(|| "manifest signature file is empty".to_string())?;
        if !untrusted.starts_with(UNTRUSTED_COMMENT_PREFIX) {
            return Err(
                "manifest signature file does not start with an untrusted comment line".to_string(),
            );
        }
        let sig_line = lines
            .next()
            .ok_or_else(|| "manifest signature file is missing the signature line".to_string())?;
        let trusted_line = lines.next().ok_or_else(|| {
            "manifest signature file is missing the trusted comment line".to_string()
        })?;
        // minisign signs exactly the bytes after "trusted comment: " (one
        // space is part of the prefix; anything after it is the comment).
        let trusted_comment = trusted_line
            .strip_prefix(TRUSTED_COMMENT_PREFIX)
            .ok_or_else(|| {
                "manifest signature file has a malformed trusted comment line".to_string()
            })?;
        let global_line = lines.next().ok_or_else(|| {
            "manifest signature file is missing the global signature line".to_string()
        })?;
        if lines.any(|line| !line.trim().is_empty()) {
            return Err("manifest signature file has unexpected trailing content".to_string());
        }

        let blob = base64::engine::general_purpose::STANDARD
            .decode(sig_line.trim())
            .map_err(|e| format!("manifest signature line is not valid base64: {e}"))?;
        if blob.len() != SIGNATURE_BLOB_LEN {
            return Err(format!(
                "manifest signature must decode to {SIGNATURE_BLOB_LEN} bytes (got {})",
                blob.len()
            ));
        }
        let prehashed = match &blob[..2] {
            alg if alg == SIG_ALG_PREHASHED => true,
            alg if alg == SIG_ALG_LEGACY => false,
            _ => return Err("manifest signature has an unsupported algorithm tag".to_string()),
        };
        let mut key_id = [0u8; KEY_ID_LEN];
        key_id.copy_from_slice(&blob[2..2 + KEY_ID_LEN]);
        let mut sig_bytes = [0u8; 64];
        sig_bytes.copy_from_slice(&blob[2 + KEY_ID_LEN..]);

        let global = base64::engine::general_purpose::STANDARD
            .decode(global_line.trim())
            .map_err(|e| format!("manifest global signature is not valid base64: {e}"))?;
        let global_bytes: [u8; 64] = global.as_slice().try_into().map_err(|_| {
            format!(
                "manifest global signature must decode to 64 bytes (got {})",
                global.len()
            )
        })?;

        Ok(Self {
            prehashed,
            key_id,
            signature: Signature::from_bytes(&sig_bytes),
            trusted_comment: trusted_comment.as_bytes().to_vec(),
            global_signature: Signature::from_bytes(&global_bytes),
        })
    }
}

/// Verify `minisig` over the exact bytes of `manifest` with one of
/// `trusted_keys`, selected by the key id the signature names.
///
/// Both the file signature and the global signature (which binds the trusted
/// comment) must verify, exactly as `minisign -V` requires. Returns the hex
/// id of the key that verified the manifest.
pub(crate) fn verify_manifest_signature(
    manifest: &[u8],
    minisig: &[u8],
    trusted_keys: &[MinisignPublicKey],
) -> Result<String, String> {
    if trusted_keys.is_empty() {
        return Err("no trusted release signing keys are configured".to_string());
    }
    let sig = MinisignSignature::parse(minisig)?;
    let signer_id = format_key_id(&sig.key_id);
    let key = trusted_keys
        .iter()
        .find(|candidate| candidate.key_id == sig.key_id)
        .ok_or_else(|| {
            let trusted = trusted_keys
                .iter()
                .map(MinisignPublicKey::key_id_hex)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "manifest is signed by minisign key {signer_id}, which is not a trusted release key (trusted: {trusted}); \
                 if the release key was rotated, upgrade via the installer, which pins the current key"
            )
        })?;

    let message: Vec<u8> = if sig.prehashed {
        use blake2::{Blake2b512, Digest};
        Blake2b512::digest(manifest).to_vec()
    } else {
        manifest.to_vec()
    };
    key.key
        .verify_strict(&message, &sig.signature)
        .map_err(|_| {
            format!(
                "minisign signature by key {signer_id} does not verify over the checksum manifest — the manifest was tampered with or does not belong to this signature"
            )
        })?;

    let mut global_message = sig.signature.to_bytes().to_vec();
    global_message.extend_from_slice(&sig.trusted_comment);
    key.key
        .verify_strict(&global_message, &sig.global_signature)
        .map_err(|_| {
            format!(
                "minisign global signature by key {signer_id} does not verify — the trusted comment was tampered with"
            )
        })?;

    Ok(signer_id)
}

/// Parse a list of minisign public keys separated by whitespace or commas.
pub(crate) fn parse_trusted_key_list(raw: &str) -> Result<Vec<MinisignPublicKey>, String> {
    let keys = raw
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(MinisignPublicKey::parse)
        .collect::<Result<Vec<_>, _>>()?;
    if keys.is_empty() {
        return Err("trusted key list is empty".to_string());
    }
    Ok(keys)
}

/// The keys the updater trusts for this process: the embedded set, or (debug
/// builds only) the [`TRUSTED_KEYS_OVERRIDE_ENV`] override.
pub(crate) fn trusted_release_keys() -> Result<Vec<MinisignPublicKey>, String> {
    let override_raw = std::env::var(TRUSTED_KEYS_OVERRIDE_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
    match override_raw {
        Some(raw) if cfg!(debug_assertions) => {
            parse_trusted_key_list(&raw).map_err(|e| format!("{TRUSTED_KEYS_OVERRIDE_ENV}: {e}"))
        }
        Some(_) => Err(format!(
            "{TRUSTED_KEYS_OVERRIDE_ENV} is set, but release builds only trust the embedded release keys; unset it to continue"
        )),
        None => TRUSTED_RELEASE_MINISIGN_KEYS
            .iter()
            .map(|k| MinisignPublicKey::parse(k))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("embedded release key is malformed: {e}")),
    }
}

/// Find the SHA-256 for `filename` in an (already authenticated) `SHA256SUMS`
/// manifest. Accepts the `sha256sum` conventions `hash  name`, `hash *name`
/// (binary mode) and `hash  ./name`; the match is on the exact file name so
/// sidecars such as `name.minisig` never match.
pub(crate) fn manifest_sha256_for(manifest: &str, filename: &str) -> Result<String, String> {
    for line in manifest.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if parts.next().is_some() {
            continue;
        }
        let name = name
            .strip_prefix('*')
            .or_else(|| name.strip_prefix("./"))
            .unwrap_or(name);
        if name != filename {
            continue;
        }
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "SHA256SUMS entry for {filename} is not a 64-hex-digit SHA-256 digest"
            ));
        }
        return Ok(hash.to_ascii_lowercase());
    }
    Err(format!("no checksum found for {filename} in SHA256SUMS"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// The real v0.3.32 release manifest and its signature, as published.
    const V0_3_32_SHA256SUMS: &[u8] =
        include_bytes!("../tests/fixtures/release_signing/v0.3.32/SHA256SUMS");
    const V0_3_32_MINISIG: &[u8] =
        include_bytes!("../tests/fixtures/release_signing/v0.3.32/SHA256SUMS.minisig");

    fn embedded_keys() -> Vec<MinisignPublicKey> {
        TRUSTED_RELEASE_MINISIGN_KEYS
            .iter()
            .map(|k| MinisignPublicKey::parse(k).expect("embedded key parses"))
            .collect()
    }

    /// A deterministic throwaway signing key plus its minisign encodings.
    struct TestKey {
        signing: SigningKey,
        key_id: [u8; KEY_ID_LEN],
    }

    impl TestKey {
        fn new(seed: u8) -> Self {
            let signing = SigningKey::from_bytes(&[seed; 32]);
            let key_id = [seed, 1, 2, 3, 4, 5, 6, 7];
            Self { signing, key_id }
        }

        fn public_line(&self) -> String {
            let mut raw = Vec::with_capacity(PUBLIC_KEY_LEN);
            raw.extend_from_slice(SIG_ALG_LEGACY);
            raw.extend_from_slice(&self.key_id);
            raw.extend_from_slice(self.signing.verifying_key().as_bytes());
            base64::engine::general_purpose::STANDARD.encode(raw)
        }

        fn public(&self) -> MinisignPublicKey {
            MinisignPublicKey::parse(&self.public_line()).expect("test key parses")
        }

        /// Produce a `.minisig` file the way `minisign -S` does (prehashed by
        /// default, legacy when `prehashed` is false).
        fn sign(&self, message: &[u8], trusted_comment: &str, prehashed: bool) -> Vec<u8> {
            let payload: Vec<u8> = if prehashed {
                use blake2::{Blake2b512, Digest};
                Blake2b512::digest(message).to_vec()
            } else {
                message.to_vec()
            };
            let signature = self.signing.sign(&payload);
            let mut blob = Vec::with_capacity(SIGNATURE_BLOB_LEN);
            blob.extend_from_slice(if prehashed {
                SIG_ALG_PREHASHED
            } else {
                SIG_ALG_LEGACY
            });
            blob.extend_from_slice(&self.key_id);
            blob.extend_from_slice(&signature.to_bytes());

            let mut global_message = signature.to_bytes().to_vec();
            global_message.extend_from_slice(trusted_comment.as_bytes());
            let global = self.signing.sign(&global_message);

            let b64 = base64::engine::general_purpose::STANDARD;
            format!(
                "untrusted comment: signature from test key\n{}\ntrusted comment: {trusted_comment}\n{}\n",
                b64.encode(blob),
                b64.encode(global.to_bytes())
            )
            .into_bytes()
        }
    }

    #[test]
    fn embedded_key_matches_installer_key_id() {
        let keys = embedded_keys();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key_id_hex(), "1BBD79B28BF718D0");
    }

    #[test]
    fn real_v0_3_32_manifest_verifies_with_embedded_key() {
        let key_id =
            verify_manifest_signature(V0_3_32_SHA256SUMS, V0_3_32_MINISIG, &embedded_keys())
                .expect("published manifest verifies");
        assert_eq!(key_id, "1BBD79B28BF718D0");
        let hash = manifest_sha256_for(
            std::str::from_utf8(V0_3_32_SHA256SUMS).unwrap(),
            "mcp-agent-mail-x86_64-unknown-linux-gnu.tar.xz",
        )
        .expect("entry present");
        assert_eq!(
            hash,
            "72628ba5c7e7d1feba9a8b8c1663937bd69da09a9ac6d90211aa537c9943a573"
        );
    }

    #[test]
    fn real_manifest_tampered_by_one_byte_fails() {
        let mut tampered = V0_3_32_SHA256SUMS.to_vec();
        // Flip a hex digit inside the first digest.
        tampered[0] = if tampered[0] == b'0' { b'1' } else { b'0' };
        let err = verify_manifest_signature(&tampered, V0_3_32_MINISIG, &embedded_keys())
            .expect_err("tampered manifest must fail");
        assert!(err.contains("does not verify"), "unexpected error: {err}");
    }

    #[test]
    fn real_manifest_with_swapped_entry_fails() {
        // A correctly formatted manifest pointing the Linux archive at a
        // different (attacker-chosen) digest, with the genuine signature.
        let text = std::str::from_utf8(V0_3_32_SHA256SUMS).unwrap();
        let swapped = text.replace(
            "72628ba5c7e7d1feba9a8b8c1663937bd69da09a9ac6d90211aa537c9943a573",
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        assert_ne!(swapped, text);
        let err = verify_manifest_signature(swapped.as_bytes(), V0_3_32_MINISIG, &embedded_keys())
            .expect_err("substituted digest must fail");
        assert!(err.contains("does not verify"), "unexpected error: {err}");
    }

    #[test]
    fn real_manifest_with_tampered_trusted_comment_fails() {
        let text = std::str::from_utf8(V0_3_32_MINISIG).unwrap();
        let tampered = text.replace("v0.3.32", "v9.9.9");
        assert_ne!(tampered, text);
        let err =
            verify_manifest_signature(V0_3_32_SHA256SUMS, tampered.as_bytes(), &embedded_keys())
                .expect_err("tampered trusted comment must fail");
        assert!(err.contains("global signature"), "unexpected error: {err}");
    }

    #[test]
    fn test_key_prehashed_and_legacy_signatures_verify() {
        let key = TestKey::new(7);
        let manifest = b"abc  mcp-agent-mail-x86_64-unknown-linux-gnu.tar.xz\n";
        for prehashed in [true, false] {
            let sig = key.sign(manifest, "test manifest", prehashed);
            let id = verify_manifest_signature(manifest, &sig, &[key.public()])
                .expect("test signature verifies");
            assert_eq!(id, key.public().key_id_hex());
        }
        // Everything after "trusted comment: " is the comment, including
        // leading whitespace and an empty comment.
        for comment in [" leading space", "", "a: b: c"] {
            let sig = key.sign(manifest, comment, true);
            verify_manifest_signature(manifest, &sig, &[key.public()])
                .unwrap_or_else(|e| panic!("comment {comment:?} must verify: {e}"));
        }
    }

    #[test]
    fn signature_from_untrusted_key_is_rejected_even_if_valid() {
        let trusted = TestKey::new(1);
        let rogue = TestKey::new(2);
        let manifest = b"manifest";
        let sig = rogue.sign(manifest, "rogue", true);
        let err = verify_manifest_signature(manifest, &sig, &[trusted.public()])
            .expect_err("untrusted key must be rejected");
        assert!(
            err.contains("not a trusted release key"),
            "unexpected error: {err}"
        );
        assert!(err.contains(&trusted.public().key_id_hex()));
    }

    #[test]
    fn key_id_collision_with_wrong_key_material_is_rejected() {
        // Same key id, different key: the id only selects the trusted key,
        // the ed25519 check still has to pass against the trusted material.
        let trusted = TestKey::new(3);
        let mut impostor = TestKey::new(4);
        impostor.key_id = trusted.key_id;
        let manifest = b"manifest";
        let sig = impostor.sign(manifest, "impostor", true);
        let err = verify_manifest_signature(manifest, &sig, &[trusted.public()])
            .expect_err("impostor must fail");
        assert!(err.contains("does not verify"), "unexpected error: {err}");
    }

    #[test]
    fn key_rotation_selects_key_by_id() {
        let old = TestKey::new(5);
        let new = TestKey::new(6);
        let manifest = b"manifest";
        let trusted = vec![old.public(), new.public()];
        let by_new = new.sign(manifest, "new", true);
        assert_eq!(
            verify_manifest_signature(manifest, &by_new, &trusted).unwrap(),
            new.public().key_id_hex()
        );
        let by_old = old.sign(manifest, "old", true);
        assert_eq!(
            verify_manifest_signature(manifest, &by_old, &trusted).unwrap(),
            old.public().key_id_hex()
        );
        // Dropping the old key from the trusted set revokes it.
        assert!(verify_manifest_signature(manifest, &by_old, &[new.public()]).is_err());
    }

    #[test]
    fn empty_trusted_set_fails_closed() {
        let key = TestKey::new(8);
        let sig = key.sign(b"m", "c", true);
        let err = verify_manifest_signature(b"m", &sig, &[]).unwrap_err();
        assert!(err.contains("no trusted release signing keys"));
    }

    #[test]
    fn malformed_signature_files_are_rejected() {
        let key = TestKey::new(9);
        let good = key.sign(b"m", "c", true);
        let good_text = std::str::from_utf8(&good).unwrap();
        let lines: Vec<&str> = good_text.lines().collect();
        let keys = [key.public()];

        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("not utf8", vec![0xff, 0xfe]),
            ("only comment", format!("{}\n", lines[0]).into_bytes()),
            (
                "missing trusted comment",
                format!("{}\n{}\n", lines[0], lines[1]).into_bytes(),
            ),
            (
                "missing global sig",
                format!("{}\n{}\n{}\n", lines[0], lines[1], lines[2]).into_bytes(),
            ),
            (
                "bad base64 sig",
                format!("{}\n@@@@\n{}\n{}\n", lines[0], lines[2], lines[3]).into_bytes(),
            ),
            (
                "short sig blob",
                format!(
                    "{}\n{}\n{}\n{}\n",
                    lines[0],
                    base64::engine::general_purpose::STANDARD.encode([0u8; 10]),
                    lines[2],
                    lines[3]
                )
                .into_bytes(),
            ),
            (
                "unknown alg",
                format!(
                    "{}\n{}\n{}\n{}\n",
                    lines[0],
                    {
                        let mut blob = base64::engine::general_purpose::STANDARD
                            .decode(lines[1])
                            .unwrap();
                        blob[0] = b'X';
                        base64::engine::general_purpose::STANDARD.encode(blob)
                    },
                    lines[2],
                    lines[3]
                )
                .into_bytes(),
            ),
            (
                "wrong comment prefix",
                format!("signed: x\n{}\n{}\n{}\n", lines[1], lines[2], lines[3]).into_bytes(),
            ),
            (
                "trailing garbage",
                format!("{good_text}extra line\n").into_bytes(),
            ),
            (
                "plain sha256sums instead of minisig",
                b"deadbeef  mcp-agent-mail.tar.xz\n".to_vec(),
            ),
        ];
        for (label, bytes) in cases {
            assert!(
                verify_manifest_signature(b"m", &bytes, &keys).is_err(),
                "{label} must be rejected"
            );
        }
        // Sanity: the untouched file still verifies, and CRLF endings are fine.
        assert!(verify_manifest_signature(b"m", &good, &keys).is_ok());
        let crlf = good_text.replace('\n', "\r\n");
        assert!(verify_manifest_signature(b"m", crlf.as_bytes(), &keys).is_ok());
    }

    #[test]
    fn public_key_parsing_rejects_garbage() {
        assert!(MinisignPublicKey::parse("").is_err());
        assert!(MinisignPublicKey::parse("not base64!").is_err());
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 10]);
        assert!(MinisignPublicKey::parse(&short).is_err());
        let mut raw = vec![b'X', b'd'];
        raw.extend_from_slice(&[0u8; 40]);
        let bad_alg = base64::engine::general_purpose::STANDARD.encode(raw);
        assert!(MinisignPublicKey::parse(&bad_alg).is_err());
    }

    #[test]
    fn trusted_key_list_parses_separators_and_rejects_empty() {
        let a = TestKey::new(10);
        let b = TestKey::new(11);
        let raw = format!("{} , {}\n", a.public_line(), b.public_line());
        let keys = parse_trusted_key_list(&raw).unwrap();
        assert_eq!(keys, vec![a.public(), b.public()]);
        assert!(parse_trusted_key_list(" , ").is_err());
        assert!(parse_trusted_key_list("garbage").is_err());
    }

    #[test]
    fn manifest_lookup_matches_exact_name_and_sha256sum_variants() {
        let hash = "72628ba5c7e7d1feba9a8b8c1663937bd69da09a9ac6d90211aa537c9943a573";
        let name = "mcp-agent-mail-x86_64-unknown-linux-gnu.tar.xz";
        for line in [
            format!("{hash}  {name}"),
            format!("{hash} *{name}"),
            format!("{hash}  ./{name}"),
            format!("{}  {name}", hash.to_uppercase()),
        ] {
            let manifest = format!("aaaa  {name}.minisig\n{line}\n");
            assert_eq!(
                manifest_sha256_for(&manifest, name).unwrap(),
                hash,
                "{line}"
            );
        }
        // Sidecar and substring names must not match.
        let manifest = format!("{hash}  {name}.minisig\n{hash}  other-{name}\n");
        assert!(manifest_sha256_for(&manifest, name).is_err());
        // A matching entry with a non-digest "hash" is refused, not trusted.
        assert!(manifest_sha256_for(&format!("nothex  {name}\n"), name).is_err());
        // Three-column lines are not sha256sum output.
        assert!(manifest_sha256_for(&format!("{hash}  {name}  extra\n"), name).is_err());
    }
}
