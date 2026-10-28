#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn am_bin() -> PathBuf {
    // Cargo provides this at compile time, including when nextest runs the tests.
    PathBuf::from(env!("CARGO_BIN_EXE_am"))
}

fn run_am(args: &[&str]) -> std::process::Output {
    Command::new(am_bin())
        .args(args)
        .env("AM_INTERFACE_MODE", "cli")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to spawn am")
}

fn write_manifest(dir: &Path, content: &str) -> PathBuf {
    let path = dir.join("manifest.json");
    std::fs::write(&path, content).expect("write manifest");
    path
}

fn write_test_signing_key(dir: &Path) -> PathBuf {
    // 32 bytes (ed25519 signing key seed).
    let key_path = dir.join("test.key");
    let key: [u8; 32] = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
        26, 27, 28, 29, 30, 31, 32,
    ];
    std::fs::write(&key_path, key).expect("write signing key");
    key_path
}

fn mutate_base64(s: &str) -> String {
    // Keep length and character set valid but ensure different string.
    let mut chars: Vec<char> = s.chars().collect();
    for c in &mut chars {
        if *c == 'A' {
            *c = 'B';
            return chars.into_iter().collect();
        }
        if c.is_ascii_alphanumeric() || *c == '+' || *c == '/' {
            *c = 'A';
            return chars.into_iter().collect();
        }
    }
    // Fallback (shouldn't happen for real base64).
    format!("{s}A")
}

#[test]
fn share_verify_ok_with_signature() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(dir.path(), r#"{"test": true}"#);
    let key_path = write_test_signing_key(dir.path());
    let sig_path = dir.path().join("manifest.sig.json");
    mcp_agent_mail_share::sign_manifest(&manifest, &key_path, &sig_path, false)
        .expect("sign manifest");

    let out = run_am(&["share", "verify", dir.path().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "expected success, got status={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn share_verify_fails_on_tampered_manifest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(dir.path(), r#"{"test": true}"#);
    let key_path = write_test_signing_key(dir.path());
    let sig_path = dir.path().join("manifest.sig.json");
    mcp_agent_mail_share::sign_manifest(&manifest, &key_path, &sig_path, false)
        .expect("sign manifest");

    // Tamper after signing.
    write_manifest(dir.path(), r#"{"test": false, "tampered": true}"#);

    let out = run_am(&["share", "verify", dir.path().to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn share_verify_public_key_override_is_used() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(dir.path(), r#"{"test": true}"#);
    let key_path = write_test_signing_key(dir.path());
    let sig_path = dir.path().join("manifest.sig.json");
    mcp_agent_mail_share::sign_manifest(&manifest, &key_path, &sig_path, false)
        .expect("sign manifest");

    let sig_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sig_path).expect("read sig"))
            .expect("parse sig json");
    let embedded = sig_json
        .get("public_key")
        .and_then(|v| v.as_str())
        .expect("embedded public_key");
    let wrong = mutate_base64(embedded);

    let out = run_am(&[
        "share",
        "verify",
        dir.path().to_str().unwrap(),
        "--public-key",
        &wrong,
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected signature failure with overridden key\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn share_decrypt_rejects_identity_plus_passphrase() {
    let dir = tempfile::tempdir().expect("tempdir");
    let encrypted = dir.path().join("bundle.zip.age");
    std::fs::write(&encrypted, b"not really age data").expect("write dummy age file");
    let identity = dir.path().join("identity.key");
    std::fs::write(&identity, b"not really an identity").expect("write dummy identity");
    let destination = dir.path().join("preserved.zip");
    std::fs::write(&destination, b"pre-existing destination").expect("write destination");

    let out = run_am(&[
        "share",
        "decrypt",
        encrypted.to_str().unwrap(),
        "-i",
        identity.to_str().unwrap(),
        "-p",
        "--output",
        destination.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(&encrypted).unwrap(), b"not really age data");
    assert_eq!(std::fs::read(&identity).unwrap(), b"not really an identity");
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        b"pre-existing destination"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("passphrase cannot be combined"),
        "stderr should mention mutual exclusion, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn share_decrypt_rejects_missing_input_without_creating_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing.age");
    let destination = dir.path().join("unpublished.zip");
    let out = run_am(&[
        "share",
        "decrypt",
        missing.to_str().unwrap(),
        "--passphrase",
        "--output",
        destination.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("encrypted file not found"));
    assert!(!missing.exists());
    assert!(!destination.exists());
}

/// R0 interoperability lane: the existing share E2E runner generates ciphertext
/// with official age and invokes this exact Cargo-built CLI. It owns the PTY
/// exchange, process timeouts and sanitized artifact receipts, so the shell and
/// Cargo lanes exercise the same real dependency contract (br-kp1in.48).
///
/// Dependency/platform absence fails by default, including release test runs.
/// Only explicit AM_SHARE_REQUIRE_AGE=0 enables optional local skips. A present
/// tool's version, key generation, encryption or decryption failure always fails.
#[cfg(unix)]
fn run_real_age_case(case: &str) {
    let required = std::env::var("AM_SHARE_REQUIRE_AGE").map_or(true, |value| value != "0");

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let artifacts = root
        .join("tests/artifacts/share_crypto")
        .join(format!("{case}-{}-{timestamp}", std::process::id()));
    let result = Command::new("bash")
        .arg(root.join("scripts/e2e_share.sh"))
        .args(["--crypto-only", case])
        .arg(am_bin())
        .env("AM_SHARE_REQUIRE_AGE", if required { "1" } else { "0" })
        .env("AM_E2E_ARTIFACT_DIR", &artifacts)
        .env("AM_E2E_KEEP_TMP", "1")
        .env("E2E_SUITE", "share_crypto")
        .env("E2E_CARGO_REQUIRE_RCH", "1")
        .env("RCH_REQUIRE_REMOTE", "1")
        .stdin(std::process::Stdio::null())
        .output();
    let out = match result {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {
            eprintln!("SKIP: bash is absent for the optional real-age lane");
            return;
        }
        result => result.expect("spawn real-age E2E runner"),
    };
    if out.status.code() == Some(77) && !required {
        eprintln!(
            "SKIP: optional real-age dependency absent; receipt: {}\n{}",
            artifacts.display(),
            String::from_utf8_lossy(&out.stdout)
        );
        return;
    }
    assert!(
        out.status.success(),
        "real-age case {case} failed (status {:?}); receipts: {}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        artifacts.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The official-age PTY fixture lane is Unix-only. Native Windows keeps real
/// identity-file interoperability without depending on a Unix shell/terminal.
#[cfg(not(unix))]
fn run_portable_identity_case(default_output: bool) {
    let required = std::env::var("AM_SHARE_REQUIRE_AGE").map_or(true, |value| value != "0");
    for tool in ["age", "age-keygen"] {
        let result = Command::new(tool) // ubs:ignore — fixed two-element executable allowlist for the required real-tool interoperability lane
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output();
        let output = match result {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {
                eprintln!("SKIP: {tool} is absent for the explicitly optional real-age lane");
                return;
            }
            result => result.expect("required real-age executable is unavailable"),
        };
        assert!(output.status.success(), "{tool} --version failed");
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let original_dir = dir.path().join("original");
    let ciphertext_dir = dir.path().join("ciphertext");
    std::fs::create_dir(&original_dir).unwrap();
    std::fs::create_dir(&ciphertext_dir).unwrap();
    let input = original_dir.join("bundle.zip");
    let mut plaintext = vec![0; 2 * 65536 + 73];
    getrandom::fill(&mut plaintext).expect("unique plaintext fixture");
    std::fs::write(&input, &plaintext).unwrap();
    let identity = dir.path().join("identity.txt");
    let keygen = Command::new("age-keygen")
        .arg("-o")
        .arg(&identity)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn age-keygen");
    assert!(keygen.status.success(), "real age-keygen failed");
    let public = Command::new("age-keygen")
        .arg("-y")
        .arg(&identity)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("derive public recipient");
    assert!(public.status.success(), "age-keygen -y failed");
    let recipient = String::from_utf8(public.stdout).expect("recipient UTF-8");
    assert!(recipient.trim().starts_with("age1"));
    let encrypted = ciphertext_dir.join("bundle.zip.age");
    let encryption = Command::new("age")
        .arg("-r")
        .arg(recipient.trim())
        .arg("-o")
        .arg(&encrypted)
        .arg(&input)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn age");
    assert!(encryption.status.success(), "real age encryption failed");
    let ciphertext = std::fs::read(&encrypted).unwrap();
    let output_path = if default_output {
        mcp_agent_mail_share::default_decrypt_output(&encrypted)
    } else {
        dir.path().join("restored.zip")
    };
    assert!(!output_path.exists());
    let mut args = vec![
        "share",
        "decrypt",
        encrypted.to_str().unwrap(),
        "--identity",
        identity.to_str().unwrap(),
    ];
    if !default_output {
        args.extend(["--output", output_path.to_str().unwrap()]);
    }
    assert!(run_am(&args).status.success(), "real am decryption failed");
    assert_eq!(std::fs::read(&output_path).unwrap(), plaintext);
    assert_eq!(run_am(&args).status.code(), Some(1), "overwrite must fail");
    assert_eq!(std::fs::read(&input).unwrap(), plaintext);
    assert_eq!(std::fs::read(&output_path).unwrap(), plaintext);
    assert_eq!(std::fs::read(&encrypted).unwrap(), ciphertext);
}

#[test]
fn share_decrypt_passphrase_preserves_whitespace_and_rejects_wrong_secret() {
    use base64::Engine as _;
    use std::io::Write as _;
    use std::process::Stdio;

    // Official age v1.2.1 ciphertext. Keep this end-to-end CLI regression
    // independent of the Rust encryptor and of an optional age executable.
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(concat!(
            "YWdlLWVuY3J5cHRpb24ub3JnL3YxCi0+IHNjcnlwdCBNZGJyRWUyMTI1OXhXZklGMDVnZVFRIDE4ClFO",
            "SjZGQnZZbnp3VlU3VVR2TDMrL21ZYUJKeTJSenZoVjFua2FtcGNGMWMKLS0tIEZWNDJqTzEwVjM0c1h0",
            "ZUFBMTlkUlRWd2ZsUi9WUys3WEpVVFdBdUR0d3MKsV8E3CJaZMgKjOXYwp4It0psXeYtkfYemRI/oVyB",
            "kWMY8joJnxn3SyEUH5cxO836hifUup82nCMgHJlSWY9Mn4K0UN8EtPuxan7KEZ8="
        ))
        .expect("official age ciphertext");
    let dir = tempfile::tempdir().expect("tempdir");
    let encrypted = dir.path().join("official.age");
    std::fs::write(&encrypted, &bytes).expect("write ciphertext");
    for (index, secret) in [
        "am-share interoperability passphrase  \t",
        "wrong passphrase",
    ]
    .iter()
    .enumerate()
    {
        let output = dir.path().join(format!("plaintext-{index}"));
        let mut child = Command::new(am_bin())
            .args([
                "share",
                "decrypt",
                encrypted.to_str().expect("UTF8 path"),
                "--passphrase",
                "--output",
                output.to_str().expect("UTF8 path"),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start actual am");
        let mut stdin = child.stdin.take().expect("passphrase pipe");
        stdin
            .write_all(secret.as_bytes())
            .expect("supply secret in memory");
        stdin.write_all(b"\r\n").expect("transport line terminator");
        drop(stdin);
        let result = child.wait_with_output().expect("wait for am");
        assert!(!String::from_utf8_lossy(&result.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&result.stderr).contains(secret));
        if index == 0 {
            assert!(
                result.status.success(),
                "passphrase decryption failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(
                std::fs::read(&output).expect("published plaintext"),
                b"Agent Mail age interoperability regression\0\xff\n"
            );
        } else {
            assert!(!result.status.success());
            assert!(!output.exists(), "wrong secret must not publish plaintext");
        }
    }
    assert_eq!(
        std::fs::read(encrypted).expect("unchanged ciphertext"),
        bytes
    );
}

#[test]
fn share_decrypt_roundtrip_identity_default_output() {
    #[cfg(unix)]
    run_real_age_case("identity-default");
    #[cfg(not(unix))]
    run_portable_identity_case(true);
}

#[test]
fn share_decrypt_roundtrip_identity_explicit_output() {
    #[cfg(unix)]
    run_real_age_case("identity-explicit");
    #[cfg(not(unix))]
    run_portable_identity_case(false);
}

#[cfg(unix)]
#[test]
fn share_decrypt_roundtrip_passphrase_preserves_whitespace() {
    run_real_age_case("passphrase");
}

#[cfg(unix)]
#[test]
fn share_decrypt_roundtrip_armored_passphrase() {
    run_real_age_case("passphrase-armored");
}

#[cfg(unix)]
#[test]
fn share_decrypt_wrong_passphrase_publishes_nothing() {
    run_real_age_case("wrong-passphrase");
}

#[cfg(unix)]
#[test]
fn share_decrypt_wrong_identity_publishes_nothing() {
    run_real_age_case("wrong-identity");
}

#[cfg(unix)]
#[test]
fn share_decrypt_tampered_passphrase_ciphertext_publishes_nothing() {
    run_real_age_case("tampered-passphrase");
}

#[cfg(unix)]
#[test]
fn share_decrypt_tampered_identity_ciphertext_publishes_nothing() {
    run_real_age_case("tampered-identity");
}

#[cfg(unix)]
#[test]
fn share_decrypt_existing_outputs_preserve_original_bytes() {
    run_real_age_case("existing-output");
}

#[cfg(unix)]
#[test]
fn share_decrypt_terminal_preserves_whitespace_without_echo() {
    run_real_age_case("passphrase-terminal");
}
