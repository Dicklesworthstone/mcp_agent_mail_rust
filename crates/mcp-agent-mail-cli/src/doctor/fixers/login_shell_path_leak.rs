//! `fm-environment_toolchain-login-shell-path-leak` — P2 detect-only.
//!
//! **Subsystem**: environment_toolchain.
//!
//! ## What's broken
//!
//! `~/.local/bin/am` is the canonical install location, but
//! interactive operators sometimes export it to `PATH` only in
//! `.bashrc` (interactive shells), while SSH-driven sessions,
//! cron jobs, and login subprocesses use non-interactive shells
//! that source only `.bash_profile` / `.zshenv`. Result: `am`
//! works in the operator's terminal but fails for `git
//! pre-commit` hooks, systemd units, and `ssh host am ...`.
//!
//! ## Detection (pure-ish — spawns shell subprocesses)
//!
//! For each shell on PATH (bash, zsh), probe four contexts:
//!
//! - `<shell> -c 'echo $PATH'`   — non-interactive non-login
//! - `<shell> -lc 'echo $PATH'`  — login shell
//!
//! For each, check whether `$HOME/.local/bin` appears in the
//! resulting `PATH`. Missing contexts produce a finding.
//!
//! Also walk a small set of rc files
//! (`.bashrc`, `.bash_profile`, `.zshrc`, `.zshenv`,
//! `.profile`) and record which ones DO export `.local/bin`.
//! This helps operators see where to add the missing export.
//!
//! ## Fix
//!
//! **Detect-only.** Editing the operator's shell rc files is
//! intentionally out of scope (RULE 1 — no file deletion;
//! similarly, no shell-rc rewrite). Manual remediation walks
//! the operator through the standard fix:
//! `export PATH="$HOME/.local/bin:$PATH"` in `.zshenv` (zsh)
//! or `.bash_profile` (bash).

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const FM_ID: &str = "fm-environment_toolchain-login-shell-path-leak";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "environment_toolchain";

/// Shells we probe. Each entry is (label, binary_name).
///
/// Intentionally narrowed to bash + zsh (the two shells the
/// project's install scripts target). `fish` and `csh` are
/// excluded because they have different startup-file conventions
/// and the failure mode this FM catches (`.bash_profile` /
/// `.zshenv` missing the PATH export) doesn't transfer cleanly
/// to those shells. Operators on other shells should add the
/// equivalent rc-file edit per their shell's documentation
/// (pass-35BB round-3 review F2, P3).
const SHELLS: &[(&str, &str)] = &[
    ("bash-noninteractive", "bash"),
    ("bash-login", "bash"),
    ("zsh-noninteractive", "zsh"),
    ("zsh-login", "zsh"),
];

/// Rc file basenames we walk for PATH export hints. Matches
/// the bash + zsh narrowing above plus `.profile` for
/// sh-compatible login shells.
const RC_FILES: &[&str] = &[".bashrc", ".bash_profile", ".zshrc", ".zshenv", ".profile"];

#[derive(Debug, Clone, Serialize)]
pub struct LoginShellPathLeakFinding {
    /// Absolute path the detector probed for (typically
    /// `~/.local/bin`).
    pub install_dir: PathBuf,
    /// Shell contexts that did NOT contain install_dir in $PATH.
    pub missing_contexts: Vec<String>,
    /// Shell contexts that DID contain install_dir (subset of
    /// the probed contexts).
    pub present_contexts: Vec<String>,
    /// Rc files mapped to "does this rc file export
    /// $HOME/.local/bin?".
    pub rc_files_with_path_export: Vec<RcFileStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RcFileStatus {
    pub path: PathBuf,
    pub exists: bool,
    pub exports_install_dir: bool,
}

impl LoginShellPathLeakFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "$HOME/.local/bin not in PATH for {} shell context(s): {}",
            self.missing_contexts.len(),
            self.missing_contexts.join(", "),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.9,
            evidence: serde_json::json!({
                "install_dir": self.install_dir.to_string_lossy(),
                "missing_contexts": self.missing_contexts,
                "present_contexts": self.present_contexts,
                "rc_files_with_path_export": self.rc_files_with_path_export,
                "manual_remediation": {
                    "steps": [
                        "For zsh users: add `export PATH=\"$HOME/.local/bin:$PATH\"` to `~/.zshenv` (covers interactive AND non-interactive shells).",
                        "For bash users: add the same line to `~/.bash_profile` (login) AND `~/.bashrc` (interactive).",
                        "For all shells: also add to `~/.profile` for sh-compatible login shells.",
                        "Reload: `exec $SHELL -l` or open a new terminal.",
                        "Verify: `bash -lc 'echo $PATH'` and `zsh -lc 'echo $PATH'` both contain ~/.local/bin.",
                    ],
                    "note": "Auto-editing operator shell rc files is intentionally out of scope (see RULE 1 in AGENTS.md). This is a guidance-only finding.",
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Inputs for the detector. Production callers leave all as
/// defaults; tests inject fabricated home + shell probe results.
#[derive(Debug, Clone, Default)]
pub struct DetectInputs {
    /// Override the home directory (production reads
    /// `dirs::home_dir()`). Set in tests to a tempdir.
    pub home_override: Option<PathBuf>,
    /// Override the install_dir basename (production uses
    /// `.local/bin`).
    pub install_subpath_override: Option<PathBuf>,
    /// Probe results injected by tests, keyed by context label.
    /// `None` in production triggers real shell-subprocess
    /// invocation.
    pub probed_paths_override: Option<Vec<(String, Option<String>)>>,
}

/// Detector. Spawns shell subprocesses (production) or reads
/// injected results (tests).
pub fn detect(inputs: &DetectInputs) -> Vec<LoginShellPathLeakFinding> {
    let Some(home) = inputs.home_override.clone().or_else(dirs::home_dir) else {
        // No home dir → nothing to probe.
        return Vec::new();
    };
    let install_subpath = inputs
        .install_subpath_override
        .clone()
        .unwrap_or_else(|| PathBuf::from(".local").join("bin"));
    let install_dir = home.join(&install_subpath);
    let install_canonical = std::fs::canonicalize(&install_dir).ok();

    // Probe shell contexts.
    let probed: Vec<(String, Option<String>)> =
        inputs.probed_paths_override.clone().unwrap_or_else(|| {
            SHELLS
                .iter()
                .map(|(label, shell)| {
                    let path_out = probe_shell_path(shell, label);
                    ((*label).to_string(), path_out)
                })
                .collect()
        });

    let mut missing = Vec::new();
    let mut present = Vec::new();
    for (label, path_str) in &probed {
        match path_str {
            Some(path_value)
                if path_contains_install_dir(
                    path_value,
                    &install_dir,
                    install_canonical.as_deref(),
                ) =>
            {
                present.push(label.clone());
            }
            Some(_) => {
                missing.push(label.clone());
            }
            None => {
                // Shell binary not present → don't count as
                // missing; just skip silently.
            }
        }
    }

    if missing.is_empty() {
        // All probed shells have install_dir in PATH; nothing
        // to flag.
        return Vec::new();
    }

    // Walk rc files for additional context.
    let rc_status: Vec<RcFileStatus> = RC_FILES
        .iter()
        .map(|name| {
            let path = home.join(name);
            let exists = path.is_file();
            let exports_install_dir = if exists {
                rc_file_exports_install_dir(&path, &install_subpath)
            } else {
                false
            };
            RcFileStatus {
                path,
                exists,
                exports_install_dir,
            }
        })
        .collect();

    vec![LoginShellPathLeakFinding {
        install_dir,
        missing_contexts: missing,
        present_contexts: present,
        rc_files_with_path_export: rc_status,
    }]
}

/// Probe the shell's PATH startup behavior with a scrubbed
/// environment so the inherited `am doctor` PATH does not bleed
/// into the result.
///
/// pass-35AA review F2 (Codex P1) fix: the previous implementation
/// used `Command::new(shell).args(...)` which inherits the parent
/// process's env. If the operator launches `am doctor` from an
/// interactive shell where `~/.local/bin` is already exported,
/// both `bash -lc 'echo $PATH'` and `zsh -lc 'echo $PATH'` would
/// inherit that PATH and report it even when the login-shell
/// startup files were missing the export — masking the exact leak
/// the detector is supposed to catch.
///
/// Fix: `.env_clear()` and explicitly set only the minimal set of
/// variables the shell needs for startup (`HOME`, `USER`,
/// `LOGNAME`, `SHELL`, `LANG`, `LC_ALL`, `TERM`). PATH is
/// intentionally NOT propagated; the shell starts fresh and rc
/// files supply the only PATH content. `HOME` is forwarded so
/// `$HOME`-relative rc-file references resolve correctly.
fn probe_shell_path(shell: &str, label: &str) -> Option<String> {
    let args: &[&str] = if label.ends_with("-login") {
        &["-lc", "echo $PATH"]
    } else {
        &["-c", "echo $PATH"]
    };
    let mut cmd = Command::new(shell);
    cmd.args(args).env_clear();
    for key in &["HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "TERM"] {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// Returns true if `path_value` (a `$PATH`-style colon-separated
/// list) contains the install_dir, comparing by canonical form
/// when possible.
fn path_contains_install_dir(
    path_value: &str,
    install_dir: &Path,
    install_canonical: Option<&Path>,
) -> bool {
    for dir in std::env::split_paths(path_value) {
        if dir == install_dir {
            return true;
        }
        if let Some(canon) = install_canonical
            && let Ok(d_canon) = std::fs::canonicalize(&dir)
            && d_canon == canon
        {
            return true;
        }
    }
    false
}

/// Scan an rc file for a line that exports `$HOME/.local/bin`
/// (or a similar pattern). Case-sensitive substring match.
fn rc_file_exports_install_dir(path: &Path, install_subpath: &Path) -> bool {
    let Ok(body) = std::fs::read_to_string(path) else {
        return false;
    };
    let install_str = install_subpath.to_string_lossy();
    // Probe several substring shapes operators commonly use.
    body.lines().any(|line| {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("export") && !trimmed.starts_with("PATH=") {
            return false;
        }
        // We're loose here: any line that exports/sets PATH AND
        // mentions `.local/bin` (or the override path) counts.
        line.contains(&*install_str)
    })
}

/// Fixer. Detect-only.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &LoginShellPathLeakFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// **NEGATIVE TEST FIRST**: every probed shell context HAS
    /// install_dir in PATH → no finding.
    #[test]
    fn detector_skips_when_all_shells_have_install_dir() {
        let td = TempDir::new().unwrap();
        let home = td.path().to_path_buf();
        let install_dir = home.join(".local").join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        let path_str = format!("/usr/bin:{}", install_dir.display());
        let inputs = DetectInputs {
            home_override: Some(home),
            install_subpath_override: None,
            probed_paths_override: Some(vec![
                ("bash-noninteractive".to_string(), Some(path_str.clone())),
                ("bash-login".to_string(), Some(path_str.clone())),
                ("zsh-noninteractive".to_string(), Some(path_str.clone())),
                ("zsh-login".to_string(), Some(path_str)),
            ]),
        };
        let findings = detect(&inputs);
        assert!(
            findings.is_empty(),
            "all-good PATH must not emit a finding; got {} finding(s)",
            findings.len()
        );
    }

    #[test]
    fn detector_flags_when_login_shell_missing_install_dir() {
        let td = TempDir::new().unwrap();
        let home = td.path().to_path_buf();
        let install_dir = home.join(".local").join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        let with = format!("/usr/bin:{}", install_dir.display());
        let without = "/usr/bin".to_string();
        let inputs = DetectInputs {
            home_override: Some(home),
            install_subpath_override: None,
            probed_paths_override: Some(vec![
                ("bash-noninteractive".to_string(), Some(with.clone())),
                ("bash-login".to_string(), Some(without.clone())),
                ("zsh-noninteractive".to_string(), Some(with.clone())),
                ("zsh-login".to_string(), Some(without)),
            ]),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.missing_contexts.len(), 2);
        assert!(f.missing_contexts.contains(&"bash-login".to_string()));
        assert!(f.missing_contexts.contains(&"zsh-login".to_string()));
        assert_eq!(f.present_contexts.len(), 2);
    }

    #[test]
    fn detector_skips_missing_shell_binaries() {
        // If a shell binary returns None (not on PATH), it's
        // not counted as missing.
        let td = TempDir::new().unwrap();
        let home = td.path().to_path_buf();
        let install_dir = home.join(".local").join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        let with = format!("/usr/bin:{}", install_dir.display());
        let inputs = DetectInputs {
            home_override: Some(home),
            install_subpath_override: None,
            probed_paths_override: Some(vec![
                ("bash-noninteractive".to_string(), Some(with.clone())),
                ("bash-login".to_string(), Some(with)),
                ("zsh-noninteractive".to_string(), None), // shell not on system
                ("zsh-login".to_string(), None),
            ]),
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    #[test]
    fn rc_file_export_detected() {
        let td = TempDir::new().unwrap();
        let rc = td.path().join(".zshenv");
        fs::write(&rc, "export PATH=\"$HOME/.local/bin:$PATH\"\n").unwrap();
        let subpath = PathBuf::from(".local").join("bin");
        assert!(rc_file_exports_install_dir(&rc, &subpath));
    }

    #[test]
    fn rc_file_no_export_skipped() {
        let td = TempDir::new().unwrap();
        let rc = td.path().join(".zshenv");
        fs::write(&rc, "# nothing here\nalias ll='ls -la'\n").unwrap();
        let subpath = PathBuf::from(".local").join("bin");
        assert!(!rc_file_exports_install_dir(&rc, &subpath));
    }

    #[test]
    fn rc_file_path_export_without_install_dir() {
        let td = TempDir::new().unwrap();
        let rc = td.path().join(".zshenv");
        fs::write(&rc, "export PATH=\"/usr/local/bin:$PATH\"\n").unwrap();
        let subpath = PathBuf::from(".local").join("bin");
        assert!(
            !rc_file_exports_install_dir(&rc, &subpath),
            "export of PATH without mentioning .local/bin must not match"
        );
    }

    #[test]
    fn path_contains_install_dir_exact_match() {
        let install = PathBuf::from("/home/x/.local/bin");
        let path = "/usr/bin:/home/x/.local/bin:/sbin";
        assert!(path_contains_install_dir(path, &install, None));
    }

    #[test]
    fn path_contains_install_dir_returns_false_when_absent() {
        let install = PathBuf::from("/home/x/.local/bin");
        let path = "/usr/bin:/sbin";
        assert!(!path_contains_install_dir(path, &install, None));
    }

    #[test]
    fn finding_serializes_with_missing_and_present_contexts() {
        let f = LoginShellPathLeakFinding {
            install_dir: "/home/x/.local/bin".into(),
            missing_contexts: vec!["bash-login".to_string()],
            present_contexts: vec!["bash-noninteractive".to_string()],
            rc_files_with_path_export: vec![],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("bash-login"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("manual_remediation"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        let td = TempDir::new().unwrap();
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), "test_run").unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = crate::doctor::mutate::MutateContext {
            run_id: "test_run".into(),
            run_dir,
            capabilities: crate::doctor::mutate::Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: std::sync::Mutex::new(actions),
            fixer_id: FM_ID.into(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: std::time::Instant::now(),
            extra_locks: Vec::new(),
        };
        let finding = LoginShellPathLeakFinding {
            install_dir: "/x".into(),
            missing_contexts: vec![],
            present_contexts: vec![],
            rc_files_with_path_export: vec![],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
