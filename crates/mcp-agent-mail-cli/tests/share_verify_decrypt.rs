#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn am_bin() -> PathBuf {
    // Cargo sets this for integration tests.
    PathBuf::from(std::env::var("CARGO_BIN_EXE_am").expect("CARGO_BIN_EXE_am must be set"))
}

fn run_am(args: &[&str]) -> std::process::Output {
    Command::new(am_bin())
        .args(args)
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

    let out = run_am(&[
        "share",
        "decrypt",
        encrypted.to_str().unwrap(),
        "-i",
        identity.to_str().unwrap(),
        "-p",
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("passphrase cannot be combined"),
        "stderr should mention mutual exclusion, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn try_generate_age_identity(dir: &Path) -> Option<(PathBuf, String)> {
    for tool in ["age", "age-keygen"] {
        match Command::new(tool).arg("--version").output() {
            // ubs:ignore -- fixed allowlist above: age and age-keygen only.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => panic!("failed to start {tool}: {error}"), // ubs:ignore -- test helper must fail for a present but broken tool.
            Ok(output) => assert!(output.status.success(), "{tool} --version failed"),
        }
    }
    let identity_path = dir.join("identity.txt");
    let out = Command::new("age-keygen")
        .arg("-o")
        .arg(&identity_path)
        .output()
        .expect("start available age-keygen");
    assert!(
        out.status.success(),
        "age-keygen failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let combined =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    let recipient = combined
        .lines()
        .find(|line| line.to_ascii_lowercase().contains("public key:"))
        .and_then(|line| line.split_whitespace().last())
        .map(str::to_string)
        .expect("age-keygen must report its public key");
    Some((identity_path, recipient))
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
    // Requires age + age-keygen CLIs.
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((identity_path, recipient)) = try_generate_age_identity(dir.path()) else {
        eprintln!("Skipping: age or age-keygen not installed");
        return;
    };

    let input = dir.path().join("bundle.zip");
    std::fs::write(&input, b"test bundle data").expect("write input");

    let generated = mcp_agent_mail_share::encrypt_with_age(&input, &[recipient])
        .expect("available age must encrypt successfully");
    let ciphertext_dir = dir.path().join("ciphertext");
    std::fs::create_dir(&ciphertext_dir).expect("create ciphertext directory");
    let encrypted = ciphertext_dir.join("bundle.zip.age");
    std::fs::rename(generated, &encrypted).expect("move ciphertext, retaining original plaintext");

    // Omit -o to exercise default output path behavior.
    let out = run_am(&[
        "share",
        "decrypt",
        encrypted.to_str().unwrap(),
        "-i",
        identity_path.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let output_path = mcp_agent_mail_share::default_decrypt_output(&encrypted);
    assert!(
        output_path.exists(),
        "expected output at {}",
        output_path.display()
    );
    let original = std::fs::read(&input).expect("read original");
    let decrypted = std::fs::read(&output_path).expect("read decrypted");
    assert_eq!(original, decrypted);
}
