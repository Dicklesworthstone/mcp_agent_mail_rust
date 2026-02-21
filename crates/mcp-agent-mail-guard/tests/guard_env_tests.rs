//! Tests for guard functions that require env var manipulation.
//!
//! Separated from lib.rs inline tests because the crate uses `#![forbid(unsafe_code)]`
//! and Rust 2024 edition makes `set_var`/`remove_var` unsafe.
#![allow(unsafe_code)]

use mcp_agent_mail_guard::{GuardError, GuardMode, guard_check, guard_check_full};
use std::path::Path;
use std::sync::Mutex;

/// Global lock to serialize env-var-mutating tests.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that saves/restores env vars on drop.
struct EnvGuard {
    vars: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    fn save(names: &[&str]) -> Self {
        let vars = names
            .iter()
            .map(|&name| (name.to_string(), std::env::var(name).ok()))
            .collect();
        Self { vars }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, saved) in &self.vars {
            match saved {
                Some(v) => unsafe { std::env::set_var(name, v) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

fn make_archive_with_reservations(td: &Path) -> std::path::PathBuf {
    let archive = td.join("archive");
    let res_dir = archive.join("file_reservations");
    std::fs::create_dir_all(&res_dir).expect("mkdir");

    let future = chrono::Utc::now() + chrono::Duration::hours(1);

    // Active exclusive reservation by OtherAgent
    let res1 = serde_json::json!({
        "path_pattern": "app/api/*.py",
        "agent_name": "OtherAgent",
        "exclusive": true,
        "expires_ts": future.to_rfc3339(),
        "released_ts": null
    });
    std::fs::write(res_dir.join("res1.json"), res1.to_string()).expect("write");

    // Self-owned reservation
    let res5 = serde_json::json!({
        "path_pattern": "my/stuff/*",
        "agent_name": "MyAgent",
        "exclusive": true,
        "expires_ts": future.to_rfc3339(),
        "released_ts": null
    });
    std::fs::write(res_dir.join("res5.json"), res5.to_string()).expect("write");

    archive
}

// -----------------------------------------------------------------------
// GuardMode::from_env tests
// -----------------------------------------------------------------------

#[test]
fn guard_mode_from_env_defaults_to_block() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_MAIL_GUARD_MODE"]);

    unsafe { std::env::remove_var("AGENT_MAIL_GUARD_MODE") };
    assert_eq!(GuardMode::from_env(), GuardMode::Block);
}

#[test]
fn guard_mode_from_env_warn() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_MAIL_GUARD_MODE"]);

    unsafe { std::env::set_var("AGENT_MAIL_GUARD_MODE", "warn") };
    assert_eq!(GuardMode::from_env(), GuardMode::Warn);
}

#[test]
fn guard_mode_from_env_warn_case_insensitive() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_MAIL_GUARD_MODE"]);

    unsafe { std::env::set_var("AGENT_MAIL_GUARD_MODE", "WARN") };
    assert_eq!(GuardMode::from_env(), GuardMode::Warn);
}

#[test]
fn guard_mode_from_env_unknown_defaults_to_block() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_MAIL_GUARD_MODE"]);

    unsafe { std::env::set_var("AGENT_MAIL_GUARD_MODE", "unknown_value") };
    assert_eq!(GuardMode::from_env(), GuardMode::Block);
}

#[test]
fn guard_mode_from_env_whitespace_trimmed() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_MAIL_GUARD_MODE"]);

    unsafe { std::env::set_var("AGENT_MAIL_GUARD_MODE", "  warn  ") };
    assert_eq!(GuardMode::from_env(), GuardMode::Warn);
}

// -----------------------------------------------------------------------
// guard_check_full tests (require env var manipulation)
// -----------------------------------------------------------------------

#[test]
fn guard_check_full_bypass_returns_empty_conflicts() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_MAIL_BYPASS"]);

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = make_archive_with_reservations(td.path());

    unsafe { std::env::set_var("AGENT_MAIL_BYPASS", "1") };
    let result =
        guard_check_full(&archive, &archive, &["app/api/users.py".to_string()]).expect("guard_check_full");
    assert!(result.bypassed);
    assert!(result.conflicts.is_empty());
}

#[test]
fn guard_check_full_gated_when_neither_flag_set() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&[
        "WORKTREES_ENABLED",
        "GIT_IDENTITY_ENABLED",
        "AGENT_MAIL_BYPASS",
    ]);

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = make_archive_with_reservations(td.path());

    unsafe {
        std::env::remove_var("WORKTREES_ENABLED");
        std::env::remove_var("GIT_IDENTITY_ENABLED");
        std::env::remove_var("AGENT_MAIL_BYPASS");
    }

    let result =
        guard_check_full(&archive, &archive, &["app/api/users.py".to_string()]).expect("guard_check_full");
    assert!(result.gated, "guard should be gated when no flags are set");
    assert!(
        result.conflicts.is_empty(),
        "gated guard should have no conflicts"
    );
}

#[test]
fn guard_check_full_missing_agent_name_returns_error() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["WORKTREES_ENABLED", "AGENT_NAME", "AGENT_MAIL_BYPASS"]);

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = make_archive_with_reservations(td.path());

    unsafe {
        std::env::set_var("WORKTREES_ENABLED", "1");
        std::env::remove_var("AGENT_NAME");
        std::env::remove_var("AGENT_MAIL_BYPASS");
    }

    let result = guard_check_full(&archive, &archive, &["app/api/users.py".to_string()]);
    assert!(result.is_err());
    assert!(
        matches!(result.unwrap_err(), GuardError::MissingAgentName),
        "expected MissingAgentName"
    );
}

#[test]
fn guard_check_full_detects_conflict_when_enabled() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["WORKTREES_ENABLED", "AGENT_NAME", "AGENT_MAIL_BYPASS"]);

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = make_archive_with_reservations(td.path());

    unsafe {
        std::env::set_var("WORKTREES_ENABLED", "1");
        std::env::set_var("AGENT_NAME", "DifferentAgent");
        std::env::remove_var("AGENT_MAIL_BYPASS");
    }

    let result =
        guard_check_full(&archive, &archive, &["app/api/users.py".to_string()]).expect("guard_check_full");
    assert!(!result.gated);
    assert!(!result.bypassed);
    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].holder, "OtherAgent");
}

#[test]
fn guard_check_full_no_conflict_for_own_reservations() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["WORKTREES_ENABLED", "AGENT_NAME", "AGENT_MAIL_BYPASS"]);

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = make_archive_with_reservations(td.path());

    unsafe {
        std::env::set_var("WORKTREES_ENABLED", "1");
        std::env::set_var("AGENT_NAME", "MyAgent");
        std::env::remove_var("AGENT_MAIL_BYPASS");
    }

    let result =
        guard_check_full(&archive, &archive, &["my/stuff/file.txt".to_string()]).expect("guard_check_full");
    assert!(
        result.conflicts.is_empty(),
        "own reservations should not conflict"
    );
}

#[test]
fn guard_check_full_empty_archive_no_conflicts() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["WORKTREES_ENABLED", "AGENT_NAME", "AGENT_MAIL_BYPASS"]);

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = td.path().join("empty_archive");
    // No file_reservations dir at all

    unsafe {
        std::env::set_var("WORKTREES_ENABLED", "1");
        std::env::set_var("AGENT_NAME", "SomeAgent");
        std::env::remove_var("AGENT_MAIL_BYPASS");
    }

    let result =
        guard_check_full(&archive, &archive, &["any/file.py".to_string()]).expect("guard_check_full");
    assert!(result.conflicts.is_empty());
}

// -----------------------------------------------------------------------
// guard_check tests (require env var manipulation)
// -----------------------------------------------------------------------

#[test]
fn guard_check_missing_agent_name_returns_error() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_NAME"]);

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = make_archive_with_reservations(td.path());

    unsafe { std::env::remove_var("AGENT_NAME") };

    let result = guard_check(&archive, &archive, &["app/api/users.py".to_string()], false);
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), GuardError::MissingAgentName));
}

#[test]
fn guard_check_detects_conflicts() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_NAME"]);

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = make_archive_with_reservations(td.path());

    unsafe { std::env::set_var("AGENT_NAME", "DifferentAgent") };

    let conflicts =
        guard_check(&archive, &archive, &["app/api/users.py".to_string()], false).expect("guard_check");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].holder, "OtherAgent");
}

#[test]
fn guard_check_no_conflicts_for_unrelated_paths() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_NAME"]);

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = make_archive_with_reservations(td.path());

    unsafe { std::env::set_var("AGENT_NAME", "SomeAgent") };

    let conflicts =
        guard_check(&archive, &archive, &["totally/unrelated.txt".to_string()], false).expect("guard_check");
    assert!(conflicts.is_empty());
}

#[test]
fn guard_check_empty_paths_no_conflicts() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_NAME"]);

    let td = tempfile::TempDir::new().expect("tempdir");
    let archive = make_archive_with_reservations(td.path());

    unsafe { std::env::set_var("AGENT_NAME", "SomeAgent") };

    let conflicts = guard_check(&archive, &archive, &[], false).expect("guard_check");
    assert!(conflicts.is_empty());
}

// -----------------------------------------------------------------------
// is_guard_gated / is_bypass_active integration tests
// -----------------------------------------------------------------------

#[test]
fn is_guard_gated_with_worktrees_enabled() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["WORKTREES_ENABLED", "GIT_IDENTITY_ENABLED"]);

    unsafe {
        std::env::set_var("WORKTREES_ENABLED", "1");
        std::env::remove_var("GIT_IDENTITY_ENABLED");
    }
    assert!(mcp_agent_mail_guard::is_guard_gated());
}

#[test]
fn is_guard_gated_with_git_identity_enabled() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["WORKTREES_ENABLED", "GIT_IDENTITY_ENABLED"]);

    unsafe {
        std::env::remove_var("WORKTREES_ENABLED");
        std::env::set_var("GIT_IDENTITY_ENABLED", "yes");
    }
    assert!(mcp_agent_mail_guard::is_guard_gated());
}

#[test]
fn is_guard_gated_false_when_neither_set() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["WORKTREES_ENABLED", "GIT_IDENTITY_ENABLED"]);

    unsafe {
        std::env::remove_var("WORKTREES_ENABLED");
        std::env::remove_var("GIT_IDENTITY_ENABLED");
    }
    assert!(!mcp_agent_mail_guard::is_guard_gated());
}

#[test]
fn is_bypass_active_when_set() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_MAIL_BYPASS"]);

    unsafe { std::env::set_var("AGENT_MAIL_BYPASS", "1") };
    assert!(mcp_agent_mail_guard::is_bypass_active());
}

#[test]
fn is_bypass_active_false_when_unset() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_MAIL_BYPASS"]);

    unsafe { std::env::remove_var("AGENT_MAIL_BYPASS") };
    assert!(!mcp_agent_mail_guard::is_bypass_active());
}

#[test]
fn is_bypass_active_false_for_zero() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::save(&["AGENT_MAIL_BYPASS"]);

    unsafe { std::env::set_var("AGENT_MAIL_BYPASS", "0") };
    assert!(!mcp_agent_mail_guard::is_bypass_active());
}
